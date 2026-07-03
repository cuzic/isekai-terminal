package tools.isekai.terminal

import uniffi.tssh_core.ScreenUpdate

data class TerminalUiState(
    val connected: Boolean = false,
    val isConnecting: Boolean = false,
    val statusMsg: String = "未接続",
    val screenUpdate: ScreenUpdate? = null,
    val lastFingerprint: String? = null,
    val scrollbackLen: Int = 0,
    val currentHost: String? = null,
    val hostKeyChangedWarning: HostKeyChangedWarning? = null,
    val trzszState: TrzszUiState? = null,
    // SSH agent forwarding: サーバー側から署名要求が来て、ユーザーの承認/拒否待ちの間だけ
    // fingerprint が入る。UI 表示だけに閉じた状態ではないが、Rust 側の oneshot 応答待ちを
    // Kotlin 側でどう見せるかという表示用のミラーであり、判断ロジック自体は
    // TerminalSession.respondAgentSignRequest() → Rust 側の oneshot で完結する。
    val agentSignRequestFingerprint: String? = null,
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
