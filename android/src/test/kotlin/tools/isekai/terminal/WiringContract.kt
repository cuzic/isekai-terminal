package tools.isekai.terminal

import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.flow.asStateFlow
import kotlinx.coroutines.flow.update
import uniffi.isekai_terminal_core.SessionOrchestratorInterface
import java.lang.reflect.InvocationTargetException
import java.lang.reflect.Proxy

/**
 * `docs/adr/0020-unwired-callback-detection.md` §3(a′)(Phase 1): 生成UniFFIバインディングの全入口・
 * 全callbackを「Androidで実際にどう配線されているか」で分類した表。
 *
 * 新しいUniFFIメソッド/関数/callbackを足したPRは、Kotlinバインディング再生成
 * (required `android-uniffi-drift`)の時点で[WiringContractTest]の網羅性assertが落ちる。
 * ここに1行足して分類すること。
 *
 * - **`pending`(「いずれ配線する」)という状態は無い**(ADR §3(f)、F8)。未配線なら未配線と
 *   [Wiring.NotUsedOnAndroid]で書き、`ref`(出典コミットのハッシュ、またはADR/ルールの`.md`節)を必須にする。
 * - `notify`で始まるメソッドは[Wiring.OsEvent]・[Wiring.UiAction]・[Wiring.NotUsedOnAndroid]にしか
 *   分類できない(ADR rev2、R2-1)。`Command`/`Query`へ逃がすとテストが落ちる。
 * - [Wiring.OsEvent]は[WiringContractTest]のライフサイクル台本で、[Wiring.UiAction]は
 *   [WiringContractComposeTest]のCompose台本で、実際に記録Proxyへ届くことを観測する。
 *   台本で観測できないとき、**分類を変えてassertを「直す」ことはしない**(台本の前提を直す、ADR §5)。
 * - [Wiring.Command]/[Wiring.Query]は本番の呼び出し元シンボル(`fqcn#method`)を必須にし、
 *   そのシンボルが実在することだけを反射で確かめる(字句的にしか確かめられないので弱い、ADR §2-1)。
 *   `observedInLifecycle = true`のものはライフサイクル台本で全paneに届くことも確かめる。
 */
sealed interface Wiring {
    /** OS/プラットフォーム由来の生イベント(ライフサイクル・ネットワーク)。台本で全paneへの到達を観測する。 */
    data object OsEvent : Wiring

    /**
     * UI操作由来の入口。[composeExemptRef]がnullならCompose台本で到達を観測する。
     * ジェスチャ専用・SAF等で意味木から駆動できないものだけ`ref`付きで個別に除外する(ADR §3(a′)-6)。
     */
    data class UiAction(val composeExemptRef: String? = null, val note: String = "") : Wiring

    /** 本番コードからの命令。[caller]は`fqcn#method`(`<init>`はコンストラクタ)。 */
    data class Command(val caller: String, val observedInLifecycle: Boolean = false) : Wiring

    /** 本番コードからの問い合わせ。[caller]は[Command]と同じ形式。 */
    data class Query(val caller: String) : Wiring

    /** このプラットフォームでは使わない。[ref]は出典コミットのハッシュかADR/ルールの`.md`節。 */
    data class NotUsedOnAndroid(val ref: String, val note: String = "") : Wiring

    // ── OrchestratorCallback(Rust → platform)側 ──

    /** callbackの結果を[TerminalUiState]等のUI状態へ反映する(到達はUI状態として既存テストで見える)。 */
    data object UiState : Wiring

    /** callbackが[tools.isekai.terminal.session.TerminalSession]の注入lambda[lambda]へ届く。到達を直接観測する。 */
    data class InjectedLambda(val lambda: String) : Wiring

    /** ログ出力のみ。[ref]必須。 */
    data class LogOnly(val ref: String) : Wiring
}

object WiringContract {
    private const val ADR = "docs/adr/0020-unwired-callback-detection.md"
    private const val HOST_PANE = "tools.isekai.terminal.TerminalHostScreenKt#TerminalPaneScreen"
    private const val COORDINATOR = "tools.isekai.terminal.ConnectionCoordinator#connectPane"
    private const val VM = "tools.isekai.terminal.TerminalTabsViewModel"
    private const val FAULT = "tools.isekai.terminal.debug.RealFaultInjectorApi"

    /** `SessionOrchestratorInterface`(UniFFI object `SessionOrchestrator`)の35メソッド。1メソッド1行、名前順。 */
    val sessionOrchestrator: Map<String, Wiring> = mapOf(
        "cancelReconnect" to Wiring.UiAction(),
        "clickToPromptCursor" to Wiring.UiAction(),
        "connect" to Wiring.Command(COORDINATOR),
        "connectIsekaiLinkRelay" to Wiring.Command(COORDINATOR),
        "connectIsekaiPipeQuic" to Wiring.Command(COORDINATOR),
        "connectIsekaiPipeQuicAuto" to Wiring.Command(COORDINATOR),
        "connectIsekaiStunP2p" to Wiring.Command(COORDINATOR),
        "connectMultipathIsekaiPipeQuic" to Wiring.Command(COORDINATOR, observedInLifecycle = true),
        "connectQuic" to Wiring.Command(COORDINATOR),
        "copyLastCommandOutput" to Wiring.UiAction(),
        "disconnect" to Wiring.Command(HOST_PANE),
        "ensureTmuxTabWindow" to Wiring.Command("$VM#maybeEnsureTmuxTabWindow"),
        "filePreviewRequest" to Wiring.UiAction(),
        "forceReturnToWifi" to Wiring.UiAction(),
        "jumpToNextPrompt" to Wiring.UiAction(),
        "jumpToPreviousPrompt" to Wiring.UiAction(),
        // db3a6d87本文: Suspended遷移経路に既知の危険なバグがあるため配線しない(Opusレビュー指摘)。iOSは配線済み。
        "notifyBackgroundBudgetExpired" to Wiring.NotUsedOnAndroid("db3a6d87", "Suspended遷移経路の既知バグのため意図的に未配線(iOSのみ)"),
        "notifyDidEnterBackground" to Wiring.OsEvent,
        "notifyFocusChange" to Wiring.UiAction(note = "TerminalScreenBodyのLaunchedEffect(isActive, hasFocus)"),
        "notifyMemoryWarning" to Wiring.NotUsedOnAndroid("db3a6d87", "notifyBackgroundBudgetExpiredと同じ理由(iOSのみ)"),
        "notifyNetworkPathChanged" to Wiring.OsEvent,
        "notifyUpstreamHealthDegraded" to Wiring.OsEvent,
        "notifyWillEnterForeground" to Wiring.OsEvent,
        "resize" to Wiring.Command(HOST_PANE),
        "scrollbackCells" to Wiring.Query(HOST_PANE),
        "scrollbackLen" to Wiring.Query("tools.isekai.terminal.session.TerminalSession#<init>"),
        "searchScrollback" to Wiring.UiAction(),
        "send" to Wiring.Command("$VM#sendSnippetToPane"),
        "setAiPanelEnabled" to Wiring.Command("$VM#onConnectionEstablished", observedInLifecycle = true),
        "setSessionTheme" to Wiring.Command("$VM#pushThemeToSession"),
        "trzszAcceptDownload" to Wiring.UiAction(),
        "trzszAcceptUpload" to Wiring.UiAction(
            composeExemptRef = "$ADR §3(a′)",
            note = "SAFのファイルピッカー(ActivityResult)経由でしか発火せずJVMのCompose台本から駆動できない。" +
                "到達はVM経由(trzszStartUploadForPane)でライフサイクル側のテストが確かめる",
        ),
        "trzszCancel" to Wiring.UiAction(),
        "trzszDismiss" to Wiring.UiAction(),
        "trzszSendChunk" to Wiring.Command("$VM#trzszStartUploadForPane"),
    )

    /** `OrchestratorCallback`(Rust → platform)の20メソッド。 */
    val orchestratorCallback: Map<String, Wiring> = mapOf(
        "onAgentSignRequest" to Wiring.UiState,
        "onClipboardPullRequest" to Wiring.InjectedLambda("onClipboardPullRequested"),
        "onClipboardWrite" to Wiring.InjectedLambda("onClipboardWriteRequested"),
        // Step 8a′(#167): 世代付き接続エッジ。TerminalSessionがconnectionEdges(Flow)へそのまま流し、
        // TerminalTabsViewModel.observeConnectionEdgesがonConnectionEstablished/onConnectionLostへ振り分ける。
        "onConnectionEdge" to Wiring.UiState,
        "onConnectionStateChanged" to Wiring.UiState,
        "onData" to Wiring.UiState,
        "onDownloadComplete" to Wiring.UiState,
        "onFilePreviewResult" to Wiring.UiState,
        // 8ced2131(PR #104)本文: Y-RではAndroidはログのみ。UX活用は別follow-up(docs/adr/0001-ios-parity-implementation.md Q10)。
        "onForegroundResume" to Wiring.LogOnly("8ced2131"),
        // e8ed36ee: add_local_forward/remove_forwardをAndroidから削除済み。状態通知はログのみ。
        "onForwardStateChanged" to Wiring.LogOnly("e8ed36ee"),
        "onHostKey" to Wiring.UiState,
        // RebindManager(Rust側FSM)が同じイベントで既にrebindするため、Kotlin側の独自rebindは撤去済み(rust-ssot.md)。
        "onNoViablePath" to Wiring.LogOnly(".claude/rules/rust-ssot.md"),
        "onNotify" to Wiring.InjectedLambda("onNotifyRequested"),
        "onPromptJump" to Wiring.UiState,
        "onPromptOutputCopyReady" to Wiring.UiState,
        "onRebindStateChanged" to Wiring.UiState,
        "onRequestCellularFd" to Wiring.InjectedLambda("acquireCellularFd"),
        "onRequestWifiFd" to Wiring.InjectedLambda("acquireWifiFd"),
        "onScreenUpdate" to Wiring.UiState,
        "onTrzszStateChanged" to Wiring.UiState,
    )

    /**
     * iOS用の診断サーフェス(UniFFI object/callback interface)。Androidは一切使わない。
     * キーは`<UniFFI名>.<メソッド>`(コンストラクタは`.new`)。
     */
    val diagnostics: Map<String, Wiring> = mapOf(
        "DiagnosticCallback.onDiagnosticEvent" to Wiring.NotUsedOnAndroid("db02bda2"),
        "DiagnosticEventQueue.drainEvents" to Wiring.NotUsedOnAndroid("47bc671b"),
        "DiagnosticEventQueue.new" to Wiring.NotUsedOnAndroid("47bc671b"),
        "DiagnosticEventQueue.push" to Wiring.NotUsedOnAndroid("47bc671b", "iOSでも呼び出し0(ADR §1.3(2)、U8)"),
        "DiagnosticEventQueue.setWakeListener" to Wiring.NotUsedOnAndroid("47bc671b"),
        "DiagnosticFrameMailbox.new" to Wiring.NotUsedOnAndroid("38560d66"),
        "DiagnosticFrameMailbox.publish" to Wiring.NotUsedOnAndroid("38560d66", "iOSでも呼び出し0(ADR §1.3(2)、U8)"),
        "DiagnosticFrameMailbox.setWakeListener" to Wiring.NotUsedOnAndroid("38560d66"),
        "DiagnosticFrameMailbox.takeLatest" to Wiring.NotUsedOnAndroid("38560d66"),
        "DiagnosticHandle.fireCallback" to Wiring.NotUsedOnAndroid("db02bda2"),
        "DiagnosticHandle.new" to Wiring.NotUsedOnAndroid("db02bda2"),
        "EventWakeListener.eventsAvailable" to Wiring.NotUsedOnAndroid("47bc671b"),
    )

    /** トップレベル関数(ファサード`Isekai_terminal_coreKt`)の27関数。 */
    val topLevelFunctions: Map<String, Wiring> = mapOf(
        "coreVersion" to Wiring.NotUsedOnAndroid("db02bda2", "iOS雛形smoke test用"),
        "corePing" to Wiring.NotUsedOnAndroid("db02bda2", "iOS雛形smoke test用"),
        "createSessionOrchestrator" to Wiring.Command("tools.isekai.terminal.session.TerminalSession#<init>"),
        "debugClearReconnectLog" to Wiring.Command("$FAULT#clearReconnectLog"),
        "debugClearReconnectPolicy" to Wiring.Command("$FAULT#clearReconnectPolicy"),
        "debugClearUdpFault" to Wiring.Command("$FAULT#clear"),
        "debugCutUdpFault" to Wiring.Command("$FAULT#cut"),
        "debugDumpReconnectLog" to Wiring.Query("$FAULT#dumpReconnectLog"),
        "debugRestoreUdpFault" to Wiring.Command("$FAULT#restore"),
        "debugSetReconnectLogPath" to Wiring.Command("tools.isekai.terminal.MainActivity#enableDebugReconnectLogIfDebugBuild"),
        "debugSetReconnectPolicy" to Wiring.Command("$FAULT#setReconnectPolicy"),
        "debugSetUdpFaultLatencyMs" to Wiring.Command("$FAULT#setLatencyMs"),
        "debugSetUdpFaultLossPermille" to Wiring.Command("$FAULT#setLossPermille"),
        "decideBatteryGuidance" to Wiring.Query("$VM#<init>"),
        // ADR §1.3(2)/U8: 両プラットフォームで呼び出し0。削除はUniFFI公開API変更を伴うので別PR。
        "reattachGraceWindowSecs" to Wiring.NotUsedOnAndroid("376d8b96", "両プラットフォームで呼び出し0(ADR §1.3(2)、U8)"),
        "reattachRecordIsFresh" to Wiring.Query("$VM#<init>"),
        // 8ced2131本文「実際の呼び出し配線は別フェーズY-P2」。その間Androidは独自のKotlinミラー
        // `TerminalTabsViewModel.tmuxClaimedProfileIds`で排他しており、書面の例外が無い
        // `rust-ssot.md`違反として記録する(ADR §3(f)・U8。Y-P2で解消)。
        "releaseTmuxWindowClaim" to Wiring.NotUsedOnAndroid("8ced2131", "rust-ssot.md違反(tmuxClaimedProfileIdsミラー)、Y-P2で解消"),
        "setCtlSocketForwardEnabled" to Wiring.Command("tools.isekai.terminal.MainActivity#restorePersistedCtlSocketForward"),
        "setTerminalTheme" to Wiring.Command("tools.isekai.terminal.MainActivity#restorePersistedTerminalTheme"),
        // f6298d23本文: AndroidはTerminalKeyEncoder.ktでRustのキー変換をミラーする確定方針(ADR §1.3(3))。
        "terminalCommitTextBytes" to Wiring.NotUsedOnAndroid("f6298d23", "TerminalKeyEncoder.ktのミラー(確定方針)"),
        "terminalCtrlByte" to Wiring.NotUsedOnAndroid("f6298d23", "TerminalKeyEncoder.ktのミラー(確定方針)"),
        "terminalKittyDisambiguatedKeyBytes" to Wiring.NotUsedOnAndroid("f6298d23", "TerminalKeyEncoder.ktのミラー(確定方針)"),
        "terminalNumpadKeyBytes" to Wiring.NotUsedOnAndroid("f6298d23", "TerminalKeyEncoder.ktのミラー(確定方針)"),
        "terminalPointerEventBytes" to Wiring.Query("tools.isekai.terminal.TerminalScreenKt#TerminalScreenBody"),
        "terminalSpecialKeyBytes" to Wiring.NotUsedOnAndroid("f6298d23", "TerminalKeyEncoder.ktのミラー(確定方針)"),
        "terminalUnicodeCharBytes" to Wiring.NotUsedOnAndroid("f6298d23", "両プラットフォームで呼び出し0(ADR §1.3(2)、U8)"),
        "tryClaimTmuxWindow" to Wiring.NotUsedOnAndroid("8ced2131", "rust-ssot.md違反(tmuxClaimedProfileIdsミラー)、Y-P2で解消"),
    )

    /** `ref`は出典コミットのハッシュか`.md`ファイル(節)への参照でなければならない(自由文だけは不可、F8)。 */
    fun isValidRef(ref: String): Boolean =
        Regex("^[0-9a-f]{7,40}$").matches(ref) || Regex("""^[\w./-]+\.md(\s+§.+)?$""").matches(ref)

    /** Kotlinのinline class引数によるJVM名マングリング(`notifyDidEnterBackground-WZ4Q5Ns`)を落とす。 */
    fun kotlinName(jvmName: String): String = jvmName.substringBefore('-')

    /** UniFFIのsnake_case名をKotlinバインディングのcamelCase名へ変換する(`connect_isekai_stun_p2p`→`connectIsekaiStunP2p`)。 */
    fun snakeToCamel(snake: String): String {
        val parts = snake.split('_').filter { it.isNotEmpty() }
        return parts.first() + parts.drop(1).joinToString("") { p -> p.replaceFirstChar { it.uppercaseChar() } }
    }
}

/**
 * Rust側(`SessionOrchestratorInterface`)だけを差し替える記録Proxy。呼ばれたメソッド名
 * (マングリングを落としたKotlin名)を記録してから[FakeOrchestrator]へそのまま委譲する。
 * 記録は別スレッド(TerminalSessionのioScope等)からも来うるので[MutableStateFlow.update]で原子的に追記し、
 * テストは[calls]を`first { ... }`で待つ(sleep/ポーリング無し)。
 */
class RecordingOrchestrator(val fake: FakeOrchestrator = FakeOrchestrator()) {
    private val _calls = MutableStateFlow<List<String>>(emptyList())
    val calls: StateFlow<List<String>> = _calls.asStateFlow()

    val proxy: SessionOrchestratorInterface = Proxy.newProxyInstance(
        SessionOrchestratorInterface::class.java.classLoader,
        arrayOf(SessionOrchestratorInterface::class.java),
    ) { self, method, args ->
        when {
            method.declaringClass == Any::class.java -> when (method.name) {
                "equals" -> self === args?.get(0)
                "hashCode" -> System.identityHashCode(self)
                else -> "RecordingOrchestrator(${System.identityHashCode(self)})"
            }
            else -> {
                _calls.update { it + WiringContract.kotlinName(method.name) }
                try {
                    method.invoke(fake, *(args ?: emptyArray()))
                } catch (e: InvocationTargetException) {
                    throw e.targetException
                }
            }
        }
    } as SessionOrchestratorInterface

    fun recorded(): Set<String> = calls.value.toSet()
}
