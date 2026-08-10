package tools.isekai.terminal

import tools.isekai.terminal.session.HostKeyChecker
import tools.isekai.terminal.session.HostKeyDecision
import uniffi.isekai_terminal_core.*

/**
 * テスト用フェイク SessionOrchestrator。
 * Rust/ネイティブを一切呼ばず、コールバックを直接発火できる。
 */
class FakeOrchestrator : SessionOrchestratorInterface {
    var callback: OrchestratorCallback? = null

    var connectCalled = false
    var connectQuicCalled = false
    var connectIsekaiPipeQuicCalled = false
    var connectIsekaiPipeQuicAutoCalled = false
    var connectMultipathIsekaiPipeQuicCalled = false
    var connectIsekaiStunP2pCalled = false
    var connectIsekaiLinkRelayCalled = false
    var disconnectCalled = false
    private var quic = false

    // 実 Rust 側の ConnPhase を模した最小限の状態。notifyNetworkLost() の
    // 判断（切断する/無視する）を Rust 側の実装に合わせてここで再現する。
    private enum class Phase { IDLE, CONNECTING, CONNECTED }
    private var phase = Phase.IDLE
    val sentBytes = mutableListOf<ByteArray>()
    var lastResizeCols: UInt? = null
    var lastResizeRows: UInt? = null
    var trzszAcceptDownloadCount = 0
    var trzszAcceptUploadCount = 0
    var trzszCancelCount = 0
    var trzszDismissCalled = false
    var forceReturnToWifiCallCount = 0
    var notifyUpstreamHealthDegradedCallCount = 0
    var cancelReconnectCalled = false

    @Throws(SshException::class)
    override fun connect(config: SshConfig) {
        connectCalled = true
        phase = Phase.CONNECTING
        callback!!.onConnectionStateChanged(ConnectionPublicState.Connecting)
    }

    @Throws(SshException::class)
    override fun connectQuic(config: QuicConfig) {
        connectQuicCalled = true
        quic = true
        phase = Phase.CONNECTING
        callback!!.onConnectionStateChanged(ConnectionPublicState.Connecting)
    }

    @Throws(SshException::class)
    override fun connectIsekaiPipeQuic(config: IsekaiPipeQuicConfig) {
        connectIsekaiPipeQuicCalled = true
        quic = true
        phase = Phase.CONNECTING
        callback!!.onConnectionStateChanged(ConnectionPublicState.Connecting)
    }

    @Throws(SshException::class)
    override fun connectIsekaiPipeQuicAuto(config: IsekaiPipeQuicConfig) {
        connectIsekaiPipeQuicAutoCalled = true
        quic = true
        phase = Phase.CONNECTING
        callback!!.onConnectionStateChanged(ConnectionPublicState.Connecting)
    }

    @Throws(SshException::class)
    override fun connectMultipathIsekaiPipeQuic(config: MultipathIsekaiPipeQuicConfig) {
        connectMultipathIsekaiPipeQuicCalled = true
        quic = true
        phase = Phase.CONNECTING
        callback!!.onConnectionStateChanged(ConnectionPublicState.Connecting)
    }

    @Throws(SshException::class)
    override fun connectIsekaiStunP2p(config: IsekaiStunP2pConfig) {
        connectIsekaiStunP2pCalled = true
        quic = true
        phase = Phase.CONNECTING
        callback!!.onConnectionStateChanged(ConnectionPublicState.Connecting)
    }

    @Throws(SshException::class)
    override fun connectIsekaiLinkRelay(config: IsekaiLinkRelayConfig) {
        connectIsekaiLinkRelayCalled = true
        quic = true
        phase = Phase.CONNECTING
        callback!!.onConnectionStateChanged(ConnectionPublicState.Connecting)
    }

    override fun disconnect() { disconnectCalled = true }
    override fun cancelReconnect() { cancelReconnectCalled = true }
    // iOSセッションライフサイクル用のRustコールバック(このファイルが対象とする複数タブ/pane
    // まわりのテストでは未検証、no-opで足りる)。
    // 実機検証(2026-07-28)のバグ修正で、TerminalTabsViewModelのファンアウトを検証
    // できるよう呼び出し回数を記録する(notifyNetworkPathChangedCallsと同じ形)。
    var notifyDidEnterBackgroundCallCount = 0
    var notifyWillEnterForegroundCallCount = 0
    override fun notifyDidEnterBackground(budgetMs: UInt) { notifyDidEnterBackgroundCallCount++ }
    override fun notifyWillEnterForeground() { notifyWillEnterForegroundCallCount++ }
    override fun notifyBackgroundBudgetExpired() {}
    override fun notifyMemoryWarning() {}
    override fun send(data: ByteArray) { sentBytes.add(data) }
    override fun resize(cols: UInt, rows: UInt) { lastResizeCols = cols; lastResizeRows = rows }
    override fun scrollbackLen(): UInt = 0u
    override fun scrollbackCells(offset: UInt, rows: UInt): List<CellData> = emptyList()
    // タスク#66: searchScrollback呼び出しの引数を記録し、任意の結果を返せるようにする
    // (TerminalSession.searchScrollbackが単純中継であることをテストで確認するため)。
    var lastSearchScrollbackQuery: String? = null
    var lastSearchScrollbackCaseSensitive: Boolean? = null
    var searchScrollbackResult: List<ScrollbackSearchMatch> = emptyList()
    override fun searchScrollback(query: String, caseSensitive: Boolean): List<ScrollbackSearchMatch> {
        lastSearchScrollbackQuery = query
        lastSearchScrollbackCaseSensitive = caseSensitive
        return searchScrollbackResult
    }
    var notifyFocusChangeCalls = mutableListOf<Boolean>()
    override fun notifyFocusChange(focused: Boolean) { notifyFocusChangeCalls.add(focused) }
    override fun trzszAcceptDownload() { trzszAcceptDownloadCount++ }
    override fun trzszAcceptUpload(fileName: String, fileSize: ULong, mode: UInt) { trzszAcceptUploadCount++ }
    override fun trzszSendChunk(data: ByteArray, isLast: Boolean) {}
    override fun trzszCancel() { trzszCancelCount++ }
    override fun forceReturnToWifi() { forceReturnToWifiCallCount++ }
    // クラッシュ観点レビュー(2026-07-31)で追加: `TerminalTabsViewModel`の
    // `forwardToRust`(OSコールバックスレッド→UniFFI境界の防御的catch)が
    // 実際に例外を握り潰すことをテストできるよう、本番のUniFFI生成
    // バインディングが投げ得る`InternalException`/`IllegalStateException`
    // (握り潰す側は`Exception`全般をcatchするので、テストではどちらでも
    // 代表できる)をここから注入できるようにする。
    var notifyUpstreamHealthDegradedError: Throwable? = null
    override fun notifyUpstreamHealthDegraded() {
        notifyUpstreamHealthDegradedCallCount++
        notifyUpstreamHealthDegradedError?.let { throw it }
    }

    // 実 Rust 側 (SessionOrchestrator::notify_network_path_changed) の判断を再現する:
    // ハンドシェイク中/プレーン TCP 接続中は切断、QUIC 接続中は無視。実装側はプレーン TCP
    // 接続中のみ 400ms debounce するが、この Fake が検証したいのはタブへの fanout など
    // Kotlin 側の配線であって debounce のタイミング自体(Rust 側で別途ユニットテスト済み)
    // ではないため、ここでは同期的に「最終的に切断されるかどうか」だけを再現する。
    // isSatisfied=true は切断判断には寄与しないが、呼び出し自体がこのペインまで届いたことは
    // notifyNetworkPathChangedCalls で検証できるようにする。
    val notifyNetworkPathChangedCalls = mutableListOf<Boolean>()
    // クラッシュ観点レビュー(2026-07-31): `notifyUpstreamHealthDegradedError`と
    // 同じ目的の注入フック。fan-out(`onNetworkPathChanged`が全ペインへ配信)の
    // 途中で1ペインが例外を投げても、他のペインへの配信が止まらないことを
    // 検証するために使う。
    var notifyNetworkPathChangedError: Throwable? = null
    override fun notifyNetworkPathChanged(isSatisfied: Boolean) {
        notifyNetworkPathChangedCalls.add(isSatisfied)
        notifyNetworkPathChangedError?.let { throw it }
        if (isSatisfied) return
        when {
            phase == Phase.CONNECTING || (phase == Phase.CONNECTED && !quic) -> {
                disconnectCalled = true
                phase = Phase.IDLE
                callback!!.onConnectionStateChanged(ConnectionPublicState.Disconnected("network lost", null))
            }
            else -> {}
        }
    }

    val setSessionThemeCalls = mutableListOf<Triple<List<UInt>, UInt, UInt>>()
    override fun setSessionTheme(ansi16: List<UInt>, defaultFg: UInt, defaultBg: UInt) {
        setSessionThemeCalls.add(Triple(ansi16, defaultFg, defaultBg))
    }

    val setAiPanelEnabledCalls = mutableListOf<Boolean>()
    override fun setAiPanelEnabled(enabled: Boolean) {
        setAiPanelEnabledCalls.add(enabled)
    }

    // ── タスク#60: tmux session group ensure/attach + ウィンドウcreate-or-select ──
    data class EnsureTmuxTabWindowCall(
        val profileIdentity: String,
        val clientId: String,
        val existingTag: String?,
        val enableNotifications: Boolean,
    )
    val ensureTmuxTabWindowCalls = mutableListOf<EnsureTmuxTabWindowCall>()
    var ensureTmuxTabWindowResult: TmuxTabWindowInfo = TmuxTabWindowInfo(
        tag = "fake-tag",
        windowIndex = 0u,
        sessionName = "fake-session",
        groupName = "fake-group",
        isNewWindow = true,
    )
    var ensureTmuxTabWindowThrows: TmuxSessionException? = null

    override suspend fun ensureTmuxTabWindow(
        profileIdentity: String,
        clientId: String,
        existingTag: String?,
        enableNotifications: Boolean,
    ): TmuxTabWindowInfo {
        ensureTmuxTabWindowCalls.add(EnsureTmuxTabWindowCall(profileIdentity, clientId, existingTag, enableNotifications))
        ensureTmuxTabWindowThrows?.let { throw it }
        return ensureTmuxTabWindowResult
    }

    // タスク#13(OSC 133)。呼び出し引数を記録するだけ(判断ロジックはRust側にあるため、
    // Fakeは配線されているかどうかのみ確認できればよい)。
    val jumpToPreviousPromptCalls = mutableListOf<Pair<UInt, Boolean>>()
    override fun jumpToPreviousPrompt(fromScrollOffset: UInt, fromShowingScrollback: Boolean) {
        jumpToPreviousPromptCalls.add(fromScrollOffset to fromShowingScrollback)
    }
    val jumpToNextPromptCalls = mutableListOf<Pair<UInt, Boolean>>()
    override fun jumpToNextPrompt(fromScrollOffset: UInt, fromShowingScrollback: Boolean) {
        jumpToNextPromptCalls.add(fromScrollOffset to fromShowingScrollback)
    }
    val clickToPromptCursorCalls = mutableListOf<Pair<UInt, UInt>>()
    override fun clickToPromptCursor(row: UInt, col: UInt) {
        clickToPromptCursorCalls.add(row to col)
    }
    var copyLastCommandOutputCallCount = 0
    override fun copyLastCommandOutput() { copyLastCommandOutputCallCount++ }

    // タスク#17(ファイルプレビュー機能)。実際の`request_id`↔要求種別のペアを記録するだけ
    // (パース/デコードロジックはRust側にあるためFakeは検証しない)。テストは
    // `simulateFilePreviewResult`で任意の[FilePreviewOutcome]を返せる。
    val filePreviewRequests = mutableListOf<Pair<String, FilePreviewRequestKind>>()
    override fun filePreviewRequest(requestId: String, kind: FilePreviewRequestKind) {
        filePreviewRequests.add(requestId to kind)
    }


    // trzszDismiss() fires Idle synchronously, matching real Rust behavior
    override fun trzszDismiss() {
        trzszDismissCalled = true
        callback!!.onTrzszStateChanged(TrzszPublicState.Idle)
    }

    // ── Simulation helpers ───────────────────────────────────────────

    fun simulateConnected(host: String = "test.host"): Unit {
        phase = Phase.CONNECTED
        callback!!.onConnectionStateChanged(ConnectionPublicState.Connected(host))
    }

    fun simulateDisconnected(reason: String? = null): Unit {
        phase = Phase.IDLE
        callback!!.onConnectionStateChanged(ConnectionPublicState.Disconnected(reason, null))
    }

    fun simulateReconnecting(elapsedSecs: UInt = 0u, timeoutSecs: UInt = 60u, reason: String? = null): Unit {
        phase = Phase.IDLE
        callback!!.onConnectionStateChanged(ConnectionPublicState.Reconnecting(elapsedSecs, timeoutSecs, reason))
    }

    fun simulateError(message: String) =
        callback!!.onConnectionStateChanged(ConnectionPublicState.Error(message))

    fun simulateData(data: ByteArray) = callback!!.onData(data)

    fun simulateHostKey(host: String = "test.host", port: UShort = 22u, fingerprint: String): Boolean =
        callback!!.onHostKey(host, port, fingerprint)

    fun simulateScreenUpdate(update: ScreenUpdate) = callback!!.onScreenUpdate(update)

    fun simulateTrzszRequest(transferId: String, mode: String, suggestedName: String?, expectedSize: ULong?) =
        callback!!.onTrzszStateChanged(TrzszPublicState.WaitingUser(transferId, mode, suggestedName, expectedSize))

    fun simulateTrzszProgress(transferId: String, transferred: ULong, total: ULong?, mode: String = "download") =
        callback!!.onTrzszStateChanged(TrzszPublicState.InProgress(transferId, mode, null, transferred, total))

    fun simulateTrzszFinished(transferId: String, success: Boolean, message: String? = null) =
        callback!!.onTrzszStateChanged(TrzszPublicState.Done(transferId, success, message))

    fun simulateDownloadComplete(fileName: String?, data: ByteArray) =
        callback!!.onDownloadComplete(fileName, data)

    fun simulateAgentSignRequest(fingerprint: String = "SHA256:test-fingerprint"): Boolean =
        callback!!.onAgentSignRequest(fingerprint)

    /** タスク#57: tmux hook発火(Rust側の抑制判断済みという前提で、届いたものとして再現)。 */
    fun simulateNotify(kind: NotifyKind) = callback!!.onNotify(kind)

    fun simulatePromptJump(target: PromptJumpTarget?) = callback!!.onPromptJump(target)

    fun simulatePromptOutputCopyReady(text: String?) = callback!!.onPromptOutputCopyReady(text)

    fun simulateFilePreviewResult(requestId: String, outcome: FilePreviewOutcome) =
        callback!!.onFilePreviewResult(requestId, outcome)
}

/** テスト用フェイク HostKeyChecker。デフォルトは常に信頼。 */
class FakeHostKeyChecker(
    private val decision: HostKeyDecision = HostKeyDecision.Trust(isNew = false),
) : HostKeyChecker {
    val checked = mutableListOf<Triple<String, Int, String>>()
    val trusted = mutableListOf<Triple<String, Int, String>>()

    override fun check(host: String, port: Int, fingerprint: String): HostKeyDecision {
        checked.add(Triple(host, port, fingerprint))
        return decision
    }

    override fun trustUpdated(host: String, port: Int, fingerprint: String) {
        trusted.add(Triple(host, port, fingerprint))
    }
}
