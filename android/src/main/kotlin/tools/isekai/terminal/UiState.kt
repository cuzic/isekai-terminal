package tools.isekai.terminal

import uniffi.isekai_terminal_core.PromptJumpTarget
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
    // タスク#13(OSC 133)「前/次のプロンプトへジャンプ」の直近の結果。
    val promptJumpResult: PromptJumpResult = PromptJumpResult(),
    // タスク#13(OSC 133)「直前コマンドの出力だけをコピー」の直近の結果。
    val promptOutputCopyResult: PromptOutputCopyResult = PromptOutputCopyResult(),
)

/** タスク#13。[PromptJumpTarget]自体は「見つからなかった」場合`null`になりうるため、
 *  単調増加する[seq]を併せて持たせる([TerminalSession.onPromptJump]のdocコメント参照
 *  ——`target`だけをComposeの`LaunchedEffect`キーにすると、連続して見つからなかった
 *  場合に値が変化せず再発火しない)。`seq == 0L`は「まだ一度もジャンプが要求されて
 *  いない」ことを表す。 */
data class PromptJumpResult(val target: PromptJumpTarget? = null, val seq: Long = 0L)

/** タスク#13。[PromptJumpResult]と同じ理由で[seq]を持つ。 */
data class PromptOutputCopyResult(val text: String? = null, val seq: Long = 0L)

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
