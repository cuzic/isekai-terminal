package tools.isekai.terminal.session

import tools.isekai.terminal.HostKeyChangedWarning
import tools.isekai.terminal.TerminalUiState
import tools.isekai.terminal.TrzszUiState
import tools.isekai.terminal.util.RemoteLogger
import kotlinx.coroutines.CompletableDeferred
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.SupervisorJob
import kotlinx.coroutines.TimeoutCancellationException
import kotlinx.coroutines.cancel
import kotlinx.coroutines.channels.Channel
import kotlinx.coroutines.flow.MutableSharedFlow
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.SharedFlow
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.flow.asSharedFlow
import kotlinx.coroutines.flow.asStateFlow
import kotlinx.coroutines.flow.update
import kotlinx.coroutines.launch
import kotlinx.coroutines.runBlocking
import kotlinx.coroutines.withTimeout
import uniffi.isekai_terminal_core.*
import java.util.concurrent.atomic.AtomicBoolean
import java.util.concurrent.atomic.AtomicLong
import java.util.concurrent.atomic.AtomicReference

/**
 * SSH セッションのドメインオブジェクト。
 *
 * [SessionOrchestrator] を薄くラップし、[OrchestratorCallback] でコールバックを受け取って
 * [TerminalUiState] に反映する。セッション状態の SSOT は Rust 側に持つ。
 */
class TerminalSession(
    private val hostKeyChecker: HostKeyChecker,
    orchestratorFactory: (OrchestratorCallback) -> SessionOrchestratorInterface = { createSessionOrchestrator(it) },
    /**
     * リモートが OSC 52 でクリップボード書き込みを要求したときに呼ばれる
     * (`ISEKAI_PIPE_DESIGN.md` §8 Epic M)。既定は no-op — 実際に Android の
     * `ClipboardManager` へ書くかどうか(opt-in設定のチェック含む)は呼び出し元の責務とし、
     * `Context` を持たないこのクラス自体には持ち込まない([RealHostKeyChecker]を
     * `TerminalTabsViewModel`側から注入するのと同じ構成)。
     */
    private val onClipboardWriteRequested: (ClipboardPayload) -> Unit = {},
    /**
     * リモートが OSC 52 query、またはtmux迂回チャンネルの`ClipboardPullRequest`で
     * クリップボードの読み出しを要求したときに呼ばれる。Rust側の`onHostKey`/
     * `onAgentSignRequest`と同じ同期ブロッキング呼び出し(Rust側の`spawn_blocking`
     * スレッドから呼ばれる)。既定はno-op(常に`null`=応答なし)。opt-in設定が無効、
     * またはクリップボードが空/取得不可なら`null`を返すこと(呼び出し元はその場合
     * デバイス側から一切応答を送らない)。
     */
    private val onClipboardPullRequested: () -> ClipboardPayload? = { null },
    /**
     * #10/#22: RebindManager(Rust側)がWiFi-bound fdを要求した。判断は一切せず、
     * 取得できたfdを返すだけ(`rust-ssot.md`準拠)。Rust側の`spawn_blocking`スレッドから
     * 同期呼び出しされる(`onHostKey`/`onAgentSignRequest`と同じ方式)。既定はno-op
     * (常に`null` — マルチパス以外のセッションでは呼ばれない)。
     */
    private val acquireWifiFd: () -> PlatformFd? = { null },
    /** 同、セルラー-bound fd版。 */
    private val acquireCellularFd: () -> PlatformFd? = { null },
    /**
     * #25: 端末ベル(BEL, 0x07)受信時に呼ばれる(可視/触覚フィードバック用)。呼び出し元は
     * [ioScope] のコルーチン上から呼ばれる(メインスレッド保証は無い)。判断ロジック
     * (取りこぼし無く1回だけ発火させる `bell_generation` の単調増加チェック)はこのクラス
     * 内で完結させ、ここでは実際にバイブレーション等を鳴らすだけの副作用注入とする
     * (`onClipboardWriteRequested` と同じ構成 — `Context` を持たないこのクラス自体には
     * 持ち込まない)。既定は no-op。
     */
    private val onBell: () -> Unit = {},
) : AutoCloseable {

    companion object {
        // Rust 側（agent_forward.rs の SIGN_CONFIRM_TIMEOUT）の 30 秒より短くして、
        // 先に Kotlin 側が拒否応答を確定できるようにする。
        private const val AGENT_SIGN_CONFIRM_TIMEOUT_MS = 25_000L
    }

    private val _state = MutableStateFlow(TerminalUiState())
    val state: StateFlow<TerminalUiState> = _state.asStateFlow()

    private val _log = MutableStateFlow("")
    val log: StateFlow<String> = _log.asStateFlow()

    private val _pendingDownloadFile = MutableStateFlow<Pair<String, ByteArray>?>(null)
    val pendingDownloadFile: StateFlow<Pair<String, ByteArray>?> = _pendingDownloadFile.asStateFlow()

    // 「WiFiはあるがupstreamが死んでいる」等、マルチパスtransportがQUIC自身の視点で
    // 「応答が一切返ってこない」ことを検知した際に発火する（Rust側`PathBroker`起点）。
    private val _noViablePathEvent = MutableSharedFlow<Unit>(extraBufferCapacity = 1)
    val noViablePathEvent: SharedFlow<Unit> = _noViablePathEvent.asSharedFlow()

    private val ioScope = CoroutineScope(SupervisorJob() + Dispatchers.IO)
    private val screenUpdateChannel = Channel<ScreenUpdate>(Channel.CONFLATED)

    private val transferAccepted = AtomicBoolean(false)

    /**
     * #25: 直近フィードバック(振動等)を発火した`ScreenUpdate.bellGeneration`。
     * `bellGeneration`はRust側`Terminal`ごとに(#24)0始まりで単調増加するカウンタなので、
     * これより大きい値を受け取った時だけ1回発火させるdedupe用の記憶として使う。
     * この[TerminalSession]インスタンス自体が1ペイン=1セッションにひも付く(`PaneState`参照)
     * ため、フィールドとして持つだけでセッション/タブ単位のキーイングになる。ただし
     * 同一ペインのまま手動`connect*()`(=[guardedConnect])が再度呼ばれる「再接続」では
     * Rust側で新しい`Terminal`が作られ`bellGeneration`が0から再スタートする(#24)ため、
     * [guardedConnect]内で新しい接続を開始する直前に同期的に0へリセットする
     * (iOS版`TerminalSessionController.reconnect()`(#26)と同じ対処、Fableレビュー指摘)。
     * ULongのビットパターンをそのまま保持するだけなので`AtomicLong`で問題ない
     * (`ULong.toLong()`/`Long.toULong()`はビット再解釈であり、full u64レンジで可逆)。
     */
    private val lastFiredBellGeneration = AtomicLong(0)

    /** [onScreenUpdate]の消費ループから呼ぶ。`bellGeneration`が進んでいれば[onBell]を1回だけ
     *  発火する(同一世代の`ScreenUpdate`がconflatedチャネル経由で再適用されても二重発火
     *  しない)。 */
    private fun maybeFireBell(update: ScreenUpdate) {
        val prev = lastFiredBellGeneration.get()
        if (update.bellGeneration > prev.toULong()) {
            lastFiredBellGeneration.set(update.bellGeneration.toLong())
            onBell()
        }
    }

    // SSH agent forwarding: 署名要求ごとにユーザー確認を待つための橋渡し。
    // Rust 側の spawn_blocking スレッドから onAgentSignRequest() が同期呼び出しされるため、
    // ここで CompletableDeferred + runBlocking を使い、UI（respondAgentSignRequest 経由）から
    // 応答が来るまでそのスレッドをブロックする（RealHostKeyChecker.check() と同じ設計）。
    private val pendingAgentSignRequest = AtomicReference<CompletableDeferred<Boolean>?>(null)

    private val callback = object : OrchestratorCallback {
        override fun onConnectionStateChanged(state: ConnectionPublicState) {
            when (state) {
                is ConnectionPublicState.Connected ->
                    RemoteLogger.i("IsekaiTerminalSSH", "✓ connected: ${state.host}")
                is ConnectionPublicState.Disconnected ->
                    RemoteLogger.i("IsekaiTerminalSSH", "✗ disconnected: reason='${state.reason ?: "none"}'")
                is ConnectionPublicState.Error ->
                    RemoteLogger.w("IsekaiTerminalSSH", "connection error: ${state.message}")
                is ConnectionPublicState.Reconnecting ->
                    RemoteLogger.i(
                        "IsekaiTerminalSSH",
                        "… reconnecting: ${state.elapsedSecs}/${state.timeoutSecs}s reason='${state.reason ?: "none"}'",
                    )
                ConnectionPublicState.Connecting -> {}
            }
            _state.update { ConnectionStateMapper.apply(it, state) }
        }

        override fun onScreenUpdate(update: ScreenUpdate) {
            if (!_state.value.connected) return
            screenUpdateChannel.trySend(update)
        }

        override fun onHostKey(host: String, port: UShort, fingerprint: String): Boolean {
            RemoteLogger.i("IsekaiTerminalSSH", "host key fingerprint: $fingerprint")
            return try {
                when (val decision = hostKeyChecker.check(host, port.toInt(), fingerprint)) {
                    is HostKeyDecision.Trust -> {
                        if (decision.isNew) {
                            RemoteLogger.i("IsekaiTerminalSSH", "TOFU: trusted $host")
                            _state.update { it.copy(lastFingerprint = fingerprint) }
                        }
                        true
                    }
                    is HostKeyDecision.Changed -> {
                        RemoteLogger.w("IsekaiTerminalSSH", "⚠ HOST KEY CHANGED: $host")
                        _state.update { it.copy(hostKeyChangedWarning = decision.warning) }
                        false
                    }
                    is HostKeyDecision.Unconfirmed -> {
                        RemoteLogger.i("IsekaiTerminalSSH", "first connection: awaiting user confirmation for $host")
                        _state.update { it.copy(newHostKeyPrompt = decision.prompt) }
                        false
                    }
                    is HostKeyDecision.Reject -> {
                        RemoteLogger.w("IsekaiTerminalSSH", "host key rejected: ${decision.reason}")
                        false
                    }
                }
            } catch (e: Exception) {
                RemoteLogger.e("IsekaiTerminalSSH", "host key check error: ${e.message}", e)
                false
            }
        }

        override fun onData(data: ByteArray) { appendLog(data) }

        override fun onTrzszStateChanged(state: TrzszPublicState) {
            // 転送が終端/中断状態(Idle・WaitingUser=新規要求・Done)に入るたびに
            // 二重起動防止フラグをリセットする(UI表示状態ではない副作用のため
            // TrzszStateMapper の対象外)。
            if (state !is TrzszPublicState.InProgress) transferAccepted.set(false)
            _state.update { it.copy(trzszState = TrzszStateMapper.toUiState(state)) }
        }

        override fun onDownloadComplete(fileName: String?, data: ByteArray) {
            _pendingDownloadFile.value = Pair(fileName ?: "download", data)
        }

        override fun onNoViablePath() {
            RemoteLogger.w("IsekaiTerminalSSH", "no viable path (QUIC sees no response on any path)")
            _noViablePathEvent.tryEmit(Unit)
        }

        override fun onForwardStateChanged(id: String, state: ForwardState) {
            when (state) {
                is ForwardState.Listening ->
                    RemoteLogger.i("IsekaiTerminalSSH", "port forward '$id': listening")
                is ForwardState.Failed ->
                    RemoteLogger.w("IsekaiTerminalSSH", "port forward '$id': failed: ${state.reason}")
                is ForwardState.Stopped ->
                    RemoteLogger.i("IsekaiTerminalSSH", "port forward '$id': stopped")
            }
        }

        override fun onClipboardWrite(payload: ClipboardPayload) {
            onClipboardWriteRequested(payload)
        }

        override fun onClipboardPullRequest(): ClipboardPayload? = onClipboardPullRequested()

        override fun onRequestWifiFd(): PlatformFd? = acquireWifiFd()

        override fun onRequestCellularFd(): PlatformFd? = acquireCellularFd()

        override fun onRebindStateChanged(state: RebindPublicState) {
            _state.update { it.copy(rebindState = state) }
        }

        // SSH agent forwarding: Rust 側の spawn_blocking スレッドから同期呼び出しされる。
        // ユーザーが respondAgentSignRequest() を呼ぶまでこのスレッドをブロックして待つ。
        // タイムアウト（Rust 側の 30 秒より短い 25 秒）した場合も拒否扱いにする。
        override fun onAgentSignRequest(keyFingerprint: String): Boolean {
            RemoteLogger.i("IsekaiTerminalSSH", "agent sign request: $keyFingerprint")
            val deferred = CompletableDeferred<Boolean>()
            pendingAgentSignRequest.set(deferred)
            _state.update { it.copy(agentSignRequestFingerprint = keyFingerprint) }
            return try {
                runBlocking {
                    try {
                        withTimeout(AGENT_SIGN_CONFIRM_TIMEOUT_MS) { deferred.await() }
                    } catch (e: TimeoutCancellationException) {
                        RemoteLogger.w("IsekaiTerminalSSH", "agent sign request timed out — denying")
                        false
                    }
                }
            } finally {
                pendingAgentSignRequest.set(null)
                _state.update { it.copy(agentSignRequestFingerprint = null) }
            }
        }
    }

    private val orchestrator: SessionOrchestratorInterface = orchestratorFactory(callback)

    init {
        ioScope.launch {
            for (update in screenUpdateChannel) {
                if (_state.value.connected) {
                    _state.update { it.copy(screenUpdate = update, scrollbackLen = orchestrator.scrollbackLen().toInt()) }
                    maybeFireBell(update)
                }
            }
        }
    }

    // ── Connection ───────────────────────────────────────────────────

    /** 各 connectXxx() 共通のガード(接続済み/接続中なら無視)とエラー処理。
     *  Rust側`SessionOrchestrator::begin_connect`が拒否するのは`Connecting`中の
     *  真の二重startのみで、`Connected`中の新規接続は(pending debounceのキャンセル+
     *  別セッションへの切り替えという内部経路のため)意図的に許可している
     *  (`orchestrator.rs`のコメント参照、Codexアーキテクチャレビューで指摘・確認済み)。
     *  ここでの`connected`チェックはRustの意思決定を先取りしているのではなく、
     *  「接続中のタブに対してUIの接続アクションから誤って新規connect_*が呼ばれない
     *  ようにする」UI側の二重サブミット防止であり、`ConnectionCoordinator.connectPane`の
     *  同種チェックとあわせて意図的に残す。 */
    private inline fun guardedConnect(connect: () -> Unit) {
        if (_state.value.let { it.connected || it.isConnecting }) return
        // #25: 新しい論理セッションを開始する直前に、直近発火済みBEL世代の記憶を
        // 同期的にリセットする([lastFiredBellGeneration]のdocコメント参照)。ここで
        // リセットすることで、この直後の`connect()`が(ハンドシェイクを経て)新しい
        // `Terminal`での最初の`ScreenUpdate`を届け始めるより確実に先行させる。
        lastFiredBellGeneration.set(0)
        // CONFLATEDチャネルに旧セッションの`ScreenUpdate`(高い`bellGeneration`)が
        // まだ未消費のまま残っている稀なケース(旧`onScreenUpdate`が切断直後の一瞬
        // `_state.value.connected`の古い読み取りで滑り込んだ場合)に備え、ここで
        // 明示的にドレインしておく。ドレインしないと、新セッション接続後にこの
        // 古いフレームが消費コルーチンでconnected判定に引っかかって適用され、
        // 新セッションの低い世代の初回BELがそれより小さいとして再び取りこぼされ得る
        // (Codexレビュー指摘)。`tryReceive()`は非suspendで、空なら何もしない。
        screenUpdateChannel.tryReceive()
        try {
            connect()
        } catch (e: SshException) {
            _state.update { it.copy(isConnecting = false, statusMsg = "エラー: ${e.message ?: "不明なエラー"}") }
        }
    }

    fun connect(config: SshConfig) = guardedConnect { orchestrator.connect(config) }

    fun connectQuic(config: QuicConfig) = guardedConnect { orchestrator.connectQuic(config) }

    /** Phase 7: 自作ヘルパー経由 QUIC。フォールバック無し（明示選択時）。 */
    fun connectIsekaiPipeQuic(config: IsekaiPipeQuicConfig) =
        guardedConnect { orchestrator.connectIsekaiPipeQuic(config) }

    /** Phase 7: 自作ヘルパー経由 QUIC を試し、失敗したら通常の TCP SSH にフォールバックする。 */
    fun connectIsekaiPipeQuicAuto(config: IsekaiPipeQuicConfig) =
        guardedConnect { orchestrator.connectIsekaiPipeQuicAuto(config) }

    /** Phase 9: 自作ヘルパー経由 QUIC + Tailscale⇔直接アドレスの受動的マルチパス。フォールバック無し。 */
    fun connectMultipathIsekaiPipeQuic(config: MultipathIsekaiPipeQuicConfig) =
        guardedConnect { orchestrator.connectMultipathIsekaiPipeQuic(config) }

    /** Phase 10: STUN+SSHランデブーによる直接P2P QUIC。relay無し・フォールバック無し。 */
    fun connectIsekaiStunP2p(config: IsekaiStunP2pConfig) =
        guardedConnect { orchestrator.connectIsekaiStunP2p(config) }

    /** Phase 10: MASQUE relay経由のP2P QUIC。フォールバック無し。 */
    fun connectIsekaiLinkRelay(config: IsekaiLinkRelayConfig) =
        guardedConnect { orchestrator.connectIsekaiLinkRelay(config) }

    fun send(bytes: ByteArray) = orchestrator.send(bytes)
    fun resize(cols: UInt, rows: UInt) = orchestrator.resize(cols, rows)

    fun disconnect() {
        _state.update { it.copy(connected = false, isConnecting = false, statusMsg = "切断済み") }
        orchestrator.disconnect()
    }

    /** 自動再接続ループ([isReconnecting]中)を中止する。判断はRust側
     *  (`SessionOrchestrator::cancelReconnect`)で行い、結果は通常の
     *  `onConnectionStateChanged`経由で[_state]に反映される。 */
    fun cancelReconnect() = orchestrator.cancelReconnect()

    fun scrollbackCells(offset: Int, rows: Int): List<CellData>? =
        orchestrator.scrollbackCells(offset.toUInt(), rows.toUInt())

    /** Phase 12: このタブだけの配色テーマを差し替える(per-session theme)。
     *  アプリ全体の既定テーマとは独立しており、以降このタブが解決するSGRにのみ反映される。 */
    fun setTheme(ansi16: List<UInt>, defaultFg: UInt, defaultBg: UInt) =
        orchestrator.setSessionTheme(ansi16, defaultFg, defaultBg)

    // ── Network ───────────────────────────────────────────────────────

    /** ネットワークpath変化イベントをそのまま Rust 側に転送する。
     *  切断するかどうか（ハンドシェイク中/TCP接続中は切断、QUIC接続中は無視。TCP接続中は
     *  瞬断で即切断しないよう debounce する）の判断はセッション状態の SSOT を持つ Rust 側
     *  （`SessionOrchestrator::notify_network_path_changed`）が行う。
     *  結果は通常の `onConnectionStateChanged` コールバック経由で [_state] に反映される。 */
    fun notifyNetworkPathChanged(isSatisfied: Boolean) = orchestrator.notifyNetworkPathChanged(isSatisfied)

    /** 「WiFiは繋がっているがupstreamが死んでいる」等を検知した際に呼ぶ。
     *  マルチパス以外のtransportや未接続時は Rust 側で無視される（日和見的に呼べばよい）。 */
    fun rebindToFd(fd: Int, localIp: String) = orchestrator.rebindToFd(fd, localIp)

    /** #11: 「今すぐWiFiに戻す」。疎通確認だけは省略されないが、静けさ待ち・セルラー
     *  最小滞在はバイパスされる(`RebindManager::handle_manual_force_return`参照)。
     *  マルチパス以外のtransportや未接続時はRust側で無視される。 */
    fun forceReturnToWifi() = orchestrator.forceReturnToWifi()

    /** #60: このペインがOS/UI上でフォーカスを得た(=タブ切替やsplit pane切替で
     *  「アクティブなタブかつフォーカス中のペイン」になった)/失ったことをそのまま
     *  Rust側へ転送する。フォーカスレポーティング(`CSI ?1004`)が有効な場合にのみ
     *  `CSI I`/`CSI O`がリモートへ送られるかどうかの判断はRust側(`Terminal`)が持つ
     *  (rust-ssot)。呼び出し元([TerminalScreenBody]の`isActive && hasFocus`)は
     *  生の可視性/フォーカス状態を渡すだけでよい。 */
    fun notifyFocusChange(focused: Boolean) = orchestrator.notifyFocusChange(focused)

    // ── Host key ──────────────────────────────────────────────────────

    fun trustUpdatedHostKey() {
        val w = _state.value.hostKeyChangedWarning ?: return
        _state.update { it.copy(hostKeyChangedWarning = null) }
        ioScope.launch {
            hostKeyChecker.trustUpdated(w.host, w.port, w.newFingerprint)
        }
    }

    fun dismissHostKeyWarning() {
        _state.update { it.copy(hostKeyChangedWarning = null) }
        disconnect()
    }

    /** 初回接続確認ダイアログで「信頼して接続」を選んだ時に呼ぶ。trust store を更新するのみで、
     *  接続自体は(ホスト鍵変更時と同様)ユーザーが手動で再接続する想定
     *  (`TerminalScreenBody`の「再接続」ボタン、`canReconnect`が true の間表示される)。 */
    fun trustNewHostKey() {
        val p = _state.value.newHostKeyPrompt ?: return
        _state.update { it.copy(newHostKeyPrompt = null) }
        ioScope.launch {
            hostKeyChecker.trustUpdated(p.host, p.port, p.fingerprint)
        }
    }

    fun dismissNewHostKeyPrompt() {
        _state.update { it.copy(newHostKeyPrompt = null) }
        disconnect()
    }

    // ── SSH agent forwarding ──────────────────────────────────────────

    /** ユーザーが署名確認ダイアログで承認/拒否を選んだ時に呼ぶ。応答が無ければ拒否扱い。 */
    fun respondAgentSignRequest(approved: Boolean) {
        val deferred = pendingAgentSignRequest.getAndSet(null) ?: return
        _state.update { it.copy(agentSignRequestFingerprint = null) }
        deferred.complete(approved)
    }

    // ── trzsz ─────────────────────────────────────────────────────────

    fun trzszAcceptDownload() {
        if (_state.value.trzszState !is TrzszUiState.WaitingUser) return
        if (!transferAccepted.compareAndSet(false, true)) return
        orchestrator.trzszAcceptDownload()
    }

    fun trzszAcceptUpload(fileName: String, fileSize: ULong, mode: UInt) {
        if (_state.value.trzszState !is TrzszUiState.WaitingUser) return
        if (!transferAccepted.compareAndSet(false, true)) return
        orchestrator.trzszAcceptUpload(fileName, fileSize, mode)
    }

    fun trzszSendChunk(data: ByteArray, isLast: Boolean) {
        orchestrator.trzszSendChunk(data, isLast)
    }

    fun trzszCancel() {
        if (_state.value.trzszState == null) return
        transferAccepted.set(false)
        _state.update { it.copy(trzszState = null) }
        orchestrator.trzszCancel()
    }

    fun trzszDismiss() = orchestrator.trzszDismiss()

    fun consumeDownloadFile() { _pendingDownloadFile.value = null }

    // ── Log ───────────────────────────────────────────────────────────

    fun clearLog() { _log.value = "" }

    private fun appendLog(bytes: ByteArray) {
        val text = bytes.toString(Charsets.UTF_8)
        _log.update { current ->
            if (current.length + text.length > 200_000) (current + text).takeLast(180_000)
            else current + text
        }
    }

    override fun close() {
        orchestrator.disconnect()
        screenUpdateChannel.close()
        ioScope.cancel()
    }
}
