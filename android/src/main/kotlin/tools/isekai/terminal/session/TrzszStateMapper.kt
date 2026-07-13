package tools.isekai.terminal.session

import tools.isekai.terminal.TrzszUiState
import uniffi.isekai_terminal_core.TrzszPublicState

/**
 * Rust側`OrchestratorCallback.onTrzszStateChanged`が届ける[TrzszPublicState]を
 * [TrzszUiState]へ変換する純粋関数([ConnectionStateMapper]と同じ理由で[TerminalSession]
 * から切り出したもの)。`transferAccepted`(二重起動防止フラグ)のリセットはUI表示状態
 * ではない副作用のため、この関数の対象外(呼び出し元の[TerminalSession]が引き続き担う)。
 */
object TrzszStateMapper {
    fun toUiState(state: TrzszPublicState): TrzszUiState? = when (state) {
        TrzszPublicState.Idle -> null
        is TrzszPublicState.WaitingUser ->
            TrzszUiState.WaitingUser(state.transferId, state.mode, state.suggestedName, state.expectedSize)
        is TrzszPublicState.InProgress ->
            TrzszUiState.InProgress(state.transferId, state.mode, state.fileName, state.transferred, state.total)
        is TrzszPublicState.Done ->
            TrzszUiState.Done(state.transferId, state.success, state.message)
    }
}
