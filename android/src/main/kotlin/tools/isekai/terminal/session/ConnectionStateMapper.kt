package tools.isekai.terminal.session

import tools.isekai.terminal.AiPanelUiState
import tools.isekai.terminal.HostKeyChangedWarning
import tools.isekai.terminal.NewHostKeyPrompt
import tools.isekai.terminal.PromptJumpResult
import tools.isekai.terminal.PromptOutputCopyResult
import tools.isekai.terminal.TerminalUiState
import uniffi.isekai_terminal_core.ConnectionPublicState
import uniffi.isekai_terminal_core.PromptJumpTarget
import uniffi.isekai_terminal_core.RebindPublicState
import uniffi.isekai_terminal_core.ScreenUpdate
import uniffi.isekai_terminal_core.TrzszPublicState

/**
 * Step 8b(`docs/adr/0019-functional-core-effects.md`): [TerminalSession]が[TerminalUiState]へ反映する
 * 更新の語彙。以前は`_state.update { it.copy(...) }`が22箇所に散らばっていたものを、
 * 1メッセージ=1つの表示上の出来事として列挙し、[ConnectionStateMapper.reduce]だけが
 * [TerminalUiState]を組み立てるようにする。
 *
 * **UI表示状態のみ**(`.claude/rules/rust-ssot.md`の例外条項)。どのメッセージも
 * 「Rustが既に下した判断/通知」または「ユーザーのUI操作」をそのまま表示へ写すだけで、
 * 接続/セッションの意思決定(いつ再接続するか・エッジ検知等)はここに持ち込まない
 * (エッジ検知はStep 8a′でRust側`on_connection_edge`へ移済み)。
 * 副作用(ログ・trust store更新・Rust呼び出し・`transferAccepted`・パネル世代のdedupe)は
 * 引き続き[TerminalSession]側に残る。
 */
sealed interface UiMsg {
    // ── 接続状態(Rust `onConnectionStateChanged`) ──
    data class ConnectionStateChanged(val state: ConnectionPublicState) : UiMsg

    /** `connect_*`がUniFFI越しに同期的に`SshException`を投げた(Rustへ届く前の失敗の表示)。 */
    data class ConnectFailed(val message: String?) : UiMsg

    /** ユーザーが切断を選んだ(`disconnect()`)。Rustの`Disconnected`通知を待たずに表示を切断済みへ。 */
    data object LocalDisconnectRequested : UiMsg

    // ── 画面 ──
    /** 消費ループが`connected`中に適用する画面更新。`scrollbackLen`は呼び出し側がRustから問い合わせ済みの値。 */
    data class ScreenUpdated(val update: ScreenUpdate, val scrollbackLen: Int) : UiMsg

    // ── ホスト鍵 ──
    data class HostKeyTrustedNew(val fingerprint: String) : UiMsg
    data class HostKeyChanged(val warning: HostKeyChangedWarning) : UiMsg
    data object HostKeyChangedWarningCleared : UiMsg
    data class NewHostKeyPromptShown(val prompt: NewHostKeyPrompt) : UiMsg
    data object NewHostKeyPromptCleared : UiMsg

    // ── trzsz ──
    data class TrzszStateChanged(val state: TrzszPublicState) : UiMsg

    /** ユーザーが転送ダイアログを取り消した(`trzszCancel()`)。 */
    data object TrzszCancelled : UiMsg

    // ── rebind ──
    data class RebindStateChanged(val state: RebindPublicState) : UiMsg

    // ── プロンプトジャンプ(OSC 133) ──
    data class PromptJumpResolved(val target: PromptJumpTarget?) : UiMsg
    data class PromptOutputCopyReady(val text: String?) : UiMsg

    // ── SSH agent forwarding ──
    data class AgentSignRequested(val keyFingerprint: String) : UiMsg
    data object AgentSignRequestCleared : UiMsg

    // ── AIパネル ──
    /** 新しい`panelGeneration`のパネル(世代のdedupeは呼び出し側で済ませてから送る)。 */
    data class AiPanelPresented(val panel: AiPanelUiState) : UiMsg
    data object AiPanelDismissed : UiMsg
}

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
    /** Step 8b: [TerminalSession]が[TerminalUiState]を更新する唯一の純粋関数。 */
    fun reduce(current: TerminalUiState, msg: UiMsg): TerminalUiState = when (msg) {
        is UiMsg.ConnectionStateChanged -> apply(current, msg.state)
        is UiMsg.ConnectFailed ->
            current.copy(isConnecting = false, statusMsg = "エラー: ${msg.message ?: "不明なエラー"}")
        // AND-M8a: 再接続ループ中([isReconnecting])の切断で「切断済み」かつReconnecting
        // という不整合な表示にならないよう、`isReconnecting`も落とす。
        UiMsg.LocalDisconnectRequested ->
            current.copy(connected = false, isConnecting = false, isReconnecting = false, statusMsg = "切断済み")
        is UiMsg.ScreenUpdated -> current.copy(screenUpdate = msg.update, scrollbackLen = msg.scrollbackLen)
        is UiMsg.HostKeyTrustedNew -> current.copy(lastFingerprint = msg.fingerprint)
        is UiMsg.HostKeyChanged -> current.copy(hostKeyChangedWarning = msg.warning)
        UiMsg.HostKeyChangedWarningCleared -> current.copy(hostKeyChangedWarning = null)
        is UiMsg.NewHostKeyPromptShown -> current.copy(newHostKeyPrompt = msg.prompt)
        UiMsg.NewHostKeyPromptCleared -> current.copy(newHostKeyPrompt = null)
        is UiMsg.TrzszStateChanged -> current.copy(trzszState = TrzszStateMapper.toUiState(msg.state))
        UiMsg.TrzszCancelled -> current.copy(trzszState = null)
        is UiMsg.RebindStateChanged -> current.copy(rebindState = msg.state)
        is UiMsg.PromptJumpResolved ->
            current.copy(promptJumpResult = PromptJumpResult(msg.target, current.promptJumpResult.seq + 1))
        is UiMsg.PromptOutputCopyReady ->
            current.copy(promptOutputCopyResult = PromptOutputCopyResult(msg.text, current.promptOutputCopyResult.seq + 1))
        is UiMsg.AgentSignRequested -> current.copy(agentSignRequestFingerprint = msg.keyFingerprint)
        UiMsg.AgentSignRequestCleared -> current.copy(agentSignRequestFingerprint = null)
        is UiMsg.AiPanelPresented -> current.copy(aiPanel = msg.panel)
        UiMsg.AiPanelDismissed -> current.copy(aiPanel = null)
    }

    /** [reduce]を順に畳み込む(テスト・将来のreplay用)。 */
    fun reduceAll(initial: TerminalUiState, msgs: Iterable<UiMsg>): TerminalUiState = msgs.fold(initial) { acc, m -> reduce(acc, m) }

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
