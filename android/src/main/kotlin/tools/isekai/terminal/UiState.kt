package tools.isekai.terminal

import uniffi.isekai_terminal_core.RebindPublicState
import uniffi.isekai_terminal_core.ScreenUpdate

data class TerminalUiState(
    val connected: Boolean = false,
    val isConnecting: Boolean = false,
    // 一度Connectedになったセッションが予期せず切断され、orchestrator(Rust側)が
    // 自動的に再接続を試みている間true(ConnectionPublicState.Reconnecting)。
    // Rustから届いた状態をそのままミラーしているだけで、ここから新たな判断ロジックは
    // 行わない(rust-ssot.md準拠、既存のconnected/isConnectingと同じ位置づけ)。
    val isReconnecting: Boolean = false,
    val statusMsg: String = "未接続",
    val screenUpdate: ScreenUpdate? = null,
    val lastFingerprint: String? = null,
    val scrollbackLen: Int = 0,
    val currentHost: String? = null,
    val hostKeyChangedWarning: HostKeyChangedWarning? = null,
    // 初回接続(Unknown host key)時、ユーザーの明示確認待ちの間だけ入る
    // (`HostKeySettings`で「初回は自動信頼」が無効な既定設定の場合)。
    val newHostKeyPrompt: NewHostKeyPrompt? = null,
    val trzszState: TrzszUiState? = null,
    // SSH agent forwarding: サーバー側から署名要求が来て、ユーザーの承認/拒否待ちの間だけ
    // fingerprint が入る。UI 表示だけに閉じた状態ではないが、Rust 側の oneshot 応答待ちを
    // Kotlin 側でどう見せるかという表示用のミラーであり、判断ロジック自体は
    // TerminalSession.respondAgentSignRequest() → Rust 側の oneshot で完結する。
    val agentSignRequestFingerprint: String? = null,
    // #19: RebindManager(Rust側)の現在状態。物理マルチパスtransport以外では常にnull。
    // 「今すぐWiFiに戻す」操作の表示可否判定に使う(UI側は推測せず、この値だけを見る、
    // rust-ssot.md準拠)。
    val rebindState: RebindPublicState? = null,
)

sealed class TrzszUiState {
    data class WaitingUser(
        val transferId: String,
        val mode: String,
        val suggestedName: String?,
        val expectedSize: ULong?,
    ) : TrzszUiState()

    data class InProgress(
        val transferId: String,
        val mode: String,
        val fileName: String?,
        val transferred: ULong,
        val total: ULong?,
    ) : TrzszUiState()

    data class Done(
        val transferId: String,
        val success: Boolean,
        val message: String?,
    ) : TrzszUiState()
}

data class HostKeyChangedWarning(
    val host: String,
    val port: Int,
    val oldFingerprint: String,
    val newFingerprint: String,
)

/** 初回接続(Unknown host key)時、ユーザーに信頼するか確認するためのプロンプト内容。 */
data class NewHostKeyPrompt(
    val host: String,
    val port: Int,
    val fingerprint: String,
)
