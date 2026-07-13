package tools.isekai.terminal.session

import tools.isekai.terminal.TerminalUiState
import uniffi.isekai_terminal_core.ConnectionPublicState

/**
 * Rust側`OrchestratorCallback.onConnectionStateChanged`が届ける[ConnectionPublicState]を、
 * 直前の[TerminalUiState]へ畳み込む(fold)純粋関数。[TerminalSession]のcallbackから
 * ロギング等の副作用を除いた「状態遷移そのもの」を切り出したもの(Android/UniFFIの
 * コールバック配線から独立してJVM単体テストできるようにする)。
 *
 * 判断ロジック自体(いつConnecting/Connected/Reconnectingになるか)はRust側SessionOrchestratorが
 * SSOTとして持つ(`.claude/rules/rust-ssot.md`)。ここはその通知を[TerminalUiState]の
 * どのフィールドへどう反映するかだけを担う、Kotlin側の表示用の畳み込みに過ぎない。
 */
object ConnectionStateMapper {
    fun apply(current: TerminalUiState, state: ConnectionPublicState): TerminalUiState =
        when (state) {
            ConnectionPublicState.Connecting ->
                current.copy(isConnecting = true, connected = false, isReconnecting = false, statusMsg = "接続中…")

            is ConnectionPublicState.Connected ->
                current.copy(
                    isConnecting = false, connected = true, isReconnecting = false,
                    statusMsg = "接続済み: ${state.host}", currentHost = state.host,
                )

            is ConnectionPublicState.Disconnected ->
                current.copy(
                    isConnecting = false, connected = false, isReconnecting = false,
                    statusMsg = state.reason?.let { r -> "切断: $r" } ?: "切断済み (不明)",
                    currentHost = null, screenUpdate = null, trzszState = null,
                )

            is ConnectionPublicState.Error ->
                current.copy(isConnecting = false, isReconnecting = false, statusMsg = "エラー: ${state.message}")

            is ConnectionPublicState.Reconnecting -> {
                val suffix = state.reason?.let { r -> " [$r]" } ?: ""
                current.copy(
                    isConnecting = false, connected = false, isReconnecting = true,
                    statusMsg = "再接続中… (${state.elapsedSecs}/${state.timeoutSecs}秒)$suffix",
                    currentHost = null, screenUpdate = null, trzszState = null,
                )
            }
        }
}
