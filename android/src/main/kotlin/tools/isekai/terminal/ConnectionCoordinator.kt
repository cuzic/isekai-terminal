package tools.isekai.terminal

import kotlinx.coroutines.CoroutineDispatcher
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.launch
import tools.isekai.terminal.data.ConnectionProfile
import tools.isekai.terminal.data.toIsekaiLinkRelayConfig
import tools.isekai.terminal.data.toIsekaiPipeQuicConfig
import tools.isekai.terminal.data.toIsekaiStunP2pConfig
import tools.isekai.terminal.data.toMultipathIsekaiPipeQuicConfig
import tools.isekai.terminal.data.toQuicConfig
import tools.isekai.terminal.data.toSshConfig
import tools.isekai.terminal.session.AppExecutor
import tools.isekai.terminal.session.AuthValidation
import tools.isekai.terminal.session.AuthValidator
import tools.isekai.terminal.session.PhysicalMultipathFds
import tools.isekai.terminal.ui.TerminalTheme
import tools.isekai.terminal.util.RemoteLogger
import uniffi.isekai_terminal_core.IsekaiLinkRelayConfig
import uniffi.isekai_terminal_core.IsekaiPipeQuicConfig
import uniffi.isekai_terminal_core.IsekaiStunP2pConfig
import uniffi.isekai_terminal_core.MultipathIsekaiPipeQuicConfig
import uniffi.isekai_terminal_core.QuicConfig
import uniffi.isekai_terminal_core.SshAuth
import uniffi.isekai_terminal_core.SshConfig
import uniffi.isekai_terminal_core.TransportPreference

/**
 * [ConnectionProfile]/[PaneState]から、トランスポート別の`connect_*`呼び出しへの分岐と、
 * 認証情報(password/公開鍵)の解決をまとめたもの。[TerminalTabsViewModel]から抽出した
 * (Task #8 段階1: `resolveAuth*`とtoConfig呼び出し周辺だけを切り出す、codexレビューの
 * 段階分割案)。タブ/ペイン構造・通知・テーマの責務は引き続き[TerminalTabsViewModel]が持つ。
 *
 * テーマ反映([pushTheme])・スニペット/打鍵列の読み込み([loadPaneContent])は呼び出し元の責務のまま
 * (このクラスはPaneState/TabStateの所有者ではない)、コールバックとして注入する。
 */
internal class ConnectionCoordinator(
    private val executor: AppExecutor,
    private val scope: CoroutineScope,
    private val ioDispatcher: CoroutineDispatcher,
    private val pushTheme: (PaneState, TerminalTheme) -> Unit,
    private val loadPaneContent: (PaneState, Long?) -> Unit,
) {
    fun connectPane(
        tabId: String,
        currentTheme: TerminalTheme,
        pane: PaneState,
        profile: ConnectionProfile,
        password: String?,
        jumpPassword: String? = null,
    ) {
        val current = pane.session.state.value
        // isReconnecting中はRust側が自動再接続ループを動かしているので、手動での
        // 二重接続を防ぐ(先にcancelReconnectPaneでループを止めてから再接続すべき)。
        // connected/isConnectingのチェックは、Rust側`begin_connect`が`Connected`中の
        // 新規接続を(内部的なtransport切り替え経路として)意図的に許可しているのとは別の
        // 目的で、「タブが接続済み/接続中の間はUIの接続アクションを無視する」という
        // UI側の二重サブミット防止(Codexアーキテクチャレビューで指摘・確認済み、
        // `TerminalSession.guardedConnect`と同種)。
        if (current.connected || current.isConnecting || current.isReconnecting) return
        // Task #10: 前回の接続試行が一度もConnectedへ遷移しないまま再接続された場合、
        // observeConnectionTransitionsのdisconnect分岐を経由しないため、ここで明示的に
        // 古いhandleを閉じてから次の接続試行に入る(閉じ忘れによるリーク防止)。
        pane.physicalMultipathHandle?.close()
        pane.physicalMultipathHandle = null
        pane.preConnectError.value = null
        armPostConnectCommands(pane, profile)
        loadPaneContent(pane, profile.id)
        RemoteLogger.i(
            "IsekaiTerminalSSH",
            "connectPane[$tabId/${pane.paneId}]: '${profile.label}' ${profile.username}@${profile.host}:${profile.port} " +
                "transport=${profile.transportPreference}" +
                (if (profile.usesJumpHost) " via jump ${profile.jumpUsername}@${profile.jumpHost}:${profile.jumpPort}" else ""),
        )
        scope.launch(ioDispatcher) {
            val auth = resolveAuth(pane, profile, password) ?: return@launch
            // 踏み台(jump host)は、SSHブートストラップを伴う全トランスポートで共通に使える
            // (TSSHD_QUICのみ旧Phase 5B経路でrust-core側が未対応、Phase 10--1c参照)。
            val jumpAuth = if (profile.usesJumpHost) {
                resolveJumpAuth(pane, profile, jumpPassword) ?: return@launch
            } else {
                null
            }
            when (profile.transportPreference) {
                TransportPreference.PLAIN_SSH -> connect(pane, profile.toSshConfig(auth, jumpAuth))
                TransportPreference.TSSHD_QUIC -> connectQuic(pane, profile.toQuicConfig(auth))
                TransportPreference.ISEKAI_PIPE_QUIC -> connectIsekaiPipeQuic(pane, profile.toIsekaiPipeQuicConfig(auth, jumpAuth))
                TransportPreference.AUTO -> connectIsekaiPipeQuicAuto(pane, profile.toIsekaiPipeQuicConfig(auth, jumpAuth))
                TransportPreference.ISEKAI_PIPE_QUIC_MULTIPATH -> {
                    // Phase 9-4（実験的機能）: 有効化されていれば物理Wi-Fi/セルラーの
                    // fdも取得してから接続する。取得に失敗/未取得でも例外にはせず、
                    // path0/path1のみのマルチパスにフォールバックする（日和見的ポリシー）。
                    val physicalFds = if (profile.enablePhysicalMultipath) {
                        val acquisition = executor.acquirePhysicalMultipathFds()
                        pane.physicalMultipathHandle = acquisition.handle
                        acquisition.fds
                    } else {
                        PhysicalMultipathFds()
                    }
                    pane.upstreamFailoverEnabledForCurrentSession = profile.enableUpstreamFailover
                    connectMultipathIsekaiPipeQuic(pane, profile.toMultipathIsekaiPipeQuicConfig(auth, physicalFds, jumpAuth))
                }
                TransportPreference.ISEKAI_STUN_P2P_QUIC ->
                    connectIsekaiStunP2p(pane, profile.toIsekaiStunP2pConfig(auth, jumpAuth))
                TransportPreference.ISEKAI_LINK_RELAY_QUIC -> {
                    // relayJwt は Room に RelayCredentialVault で暗号化して保存してあるため、
                    // 実際の接続直前に復号する(toIsekaiLinkRelayConfig 自体は暗号化を意識しない
                    // 純粋なマッピング関数のまま保つ)。
                    val decrypted = profile.copy(relayJwt = profile.relayJwt?.let { executor.decryptRelayJwt(it) })
                    connectIsekaiLinkRelay(pane, decrypted.toIsekaiLinkRelayConfig(auth, jumpAuth))
                }
            }
            // タスク#65: 復号済み秘密鍵PEMのベストエフォートなメモリ消去。
            // connect_* はUniFFI越しのFFI呼び出しで、呼び出し内でByteArrayの内容を
            // 同期的にRust側へコピーしてから戻る(直上のコメント参照)ため、
            // ここで元のByteArrayをゼロ埋めしてもRust側の認証には影響しない。
            // ただしJVM上に他の参照(GCされるまでのコピー等)が残っていないことまでは
            // 保証できないベストエフォートの対策。
            wipeIfPublicKey(auth)
            wipeIfPublicKey(jumpAuth)
            // Phase 12 P2-1: このタブが解決したテーマ(Global default → Profile default)を
            // 接続直後に反映する。connect_* はRust側で同期的にActiveSessionを差し込むため、
            // このタイミングで呼べば確実にセッションへ届く。分割ペインも含め、タブ内の
            // 全ペインに同じテーマを適用する(ペイン単位の配色分岐はスコープ外)。
            pushTheme(pane, currentTheme)
        }
    }

    /** [auth]が公開鍵認証なら復号済みPEMのByteArrayをその場でゼロ埋めする(タスク#65)。
     *  パスワード認証の`String`は不変かつCompose `TextField`がString前提のため、
     *  完全なゼロ化は行わない(ベストエフォート対策として本コメントで言及するに留める)。 */
    private fun wipeIfPublicKey(auth: SshAuth?) {
        if (auth is SshAuth.PublicKey) {
            java.util.Arrays.fill(auth.privateKeyPem, 0)
        }
    }

    /** 新しい接続試行のたびに呼び、この接続で送るべきコマンド（あれば）とフラグをリセットする。 */
    private fun armPostConnectCommands(pane: PaneState, profile: ConnectionProfile) {
        val commands = profile.postConnectCommands?.takeIf { it.isNotBlank() }
        pane.pendingPostConnectBytes = commands?.let { SnippetCommands.toBytes(it, appendNewline = true) }
        pane.postConnectSent.set(pane.pendingPostConnectBytes == null)
    }

    private fun connect(pane: PaneState, config: SshConfig) {
        executor.ensureServiceRunning()
        pane.session.connect(config)
    }

    private fun connectQuic(pane: PaneState, config: QuicConfig) {
        executor.ensureServiceRunning()
        pane.session.connectQuic(config)
    }

    private fun connectIsekaiPipeQuic(pane: PaneState, config: IsekaiPipeQuicConfig) {
        executor.ensureServiceRunning()
        pane.session.connectIsekaiPipeQuic(config)
    }

    private fun connectIsekaiPipeQuicAuto(pane: PaneState, config: IsekaiPipeQuicConfig) {
        executor.ensureServiceRunning()
        pane.session.connectIsekaiPipeQuicAuto(config)
    }

    private fun connectMultipathIsekaiPipeQuic(pane: PaneState, config: MultipathIsekaiPipeQuicConfig) {
        executor.ensureServiceRunning()
        pane.session.connectMultipathIsekaiPipeQuic(config)
    }

    private fun connectIsekaiStunP2p(pane: PaneState, config: IsekaiStunP2pConfig) {
        executor.ensureServiceRunning()
        pane.session.connectIsekaiStunP2p(config)
    }

    private fun connectIsekaiLinkRelay(pane: PaneState, config: IsekaiLinkRelayConfig) {
        executor.ensureServiceRunning()
        pane.session.connectIsekaiLinkRelay(config)
    }

    private suspend fun resolveAuth(pane: PaneState, profile: ConnectionProfile, password: String?): SshAuth? =
        resolveAuthInternal(pane, profile.authType, password, profile.keyId, errorPrefix = "")

    /** 踏み台(jump host)側の認証情報を解決する。[resolveAuth] と同じ検証ロジックを
     *  jump_auth_type/jump_key_id に適用するだけの対の関数。 */
    private suspend fun resolveJumpAuth(pane: PaneState, profile: ConnectionProfile, jumpPassword: String?): SshAuth? =
        resolveAuthInternal(pane, profile.jumpAuthType ?: "", jumpPassword, profile.jumpKeyId, errorPrefix = "踏み台: ")

    private suspend fun resolveAuthInternal(
        pane: PaneState,
        authType: String,
        password: String?,
        keyId: Long?,
        errorPrefix: String,
    ): SshAuth? {
        return when (val v = AuthValidator.validate(authType, password, keyId)) {
            is AuthValidation.Error -> {
                RemoteLogger.w("IsekaiTerminalSSH", "${errorPrefix}auth error: ${v.statusMsg}")
                pane.preConnectError.value = "$errorPrefix${v.statusMsg}"
                null
            }
            is AuthValidation.Password -> SshAuth.Password(v.value)
            is AuthValidation.PublicKey -> loadPublicKeyAuth(pane, v.keyId)
        }
    }

    private suspend fun loadPublicKeyAuth(pane: PaneState, keyId: Long): SshAuth? =
        runCatching { SshAuth.PublicKey(executor.loadKeyPem(keyId)) }
            .getOrElse { e ->
                RemoteLogger.e("IsekaiTerminalSSH", "key error: ${e.message}", e)
                pane.preConnectError.value = "鍵エラー: ${e.message}"
                null
            }
}
