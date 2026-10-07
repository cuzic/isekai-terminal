package tools.isekai.terminal.session

import org.junit.Assert.assertEquals
import org.junit.Assert.assertNull
import org.junit.Assert.assertSame
import org.junit.Assert.assertTrue
import org.junit.Test
import tools.isekai.terminal.AiPanelUiState
import tools.isekai.terminal.HostKeyChangedWarning
import tools.isekai.terminal.NewHostKeyPrompt
import tools.isekai.terminal.PromptJumpResult
import tools.isekai.terminal.PromptOutputCopyResult
import tools.isekai.terminal.TerminalUiState
import tools.isekai.terminal.TrzszUiState
import uniffi.isekai_terminal_core.ConnectionPublicState
import uniffi.isekai_terminal_core.CursorShape
import uniffi.isekai_terminal_core.MouseReportingMode
import uniffi.isekai_terminal_core.NotifyKind
import uniffi.isekai_terminal_core.PanelField
import uniffi.isekai_terminal_core.PanelFieldKind
import uniffi.isekai_terminal_core.PanelKind
import uniffi.isekai_terminal_core.PromptJumpTarget
import uniffi.isekai_terminal_core.RebindPublicState
import uniffi.isekai_terminal_core.ScreenUpdate
import uniffi.isekai_terminal_core.TrzszPublicState
import kotlin.random.Random

/**
 * Step 8b(`docs/adr/0019-functional-core-effects.md`): [ConnectionStateMapper.reduce]の検証。
 *
 * - 表テスト: 各[UiMsg]が[TerminalUiState]のどのフィールドを変え、どれを変えないか。
 * - 性質テスト: 任意のメッセージ列を[ConnectionStateMapper.reduceAll]で畳み込んだ結果が、
 *   Step 8b以前の`TerminalSession.kt`に散らばっていた`_state.update { it.copy(...) }`22箇所を
 *   そのまま書き写した[legacyApply]で畳み込んだ結果と、全ての途中状態で一致すること
 *   (=挙動不変)。
 */
class UiMsgReducerTest {

    private fun screenUpdate(gen: ULong = 0uL) = ScreenUpdate(
        0u, 80u, 24u, emptyList(), 0u, 0u, null, null, null, null, false, false, false,
        MouseReportingMode.OFF, false, false, false, true, gen, 0uL, NotifyKind.INFO, "", "", 0uL, PanelKind.NONE, "", "", emptyList(), CursorShape.BLOCK, true, emptyList(),
        emptyList(), 0u, null)

    private val warning = HostKeyChangedWarning("h", 22, "old", "new")
    private val prompt = NewHostKeyPrompt("h", 22, "fp")
    private val panel = AiPanelUiState(PanelKind.DOCUMENT, "t", "# md", emptyList())

    /** 全フィールドが非既定値の状態(「触らないはずのフィールドを触っていない」の検出用)。 */
    private val busy = TerminalUiState(
        connected = true, isConnecting = true, isReconnecting = true, statusMsg = "busy",
        screenUpdate = screenUpdate(), lastFingerprint = "lf", scrollbackLen = 7, currentHost = "host",
        hostKeyChangedWarning = warning, newHostKeyPrompt = prompt,
        trzszState = TrzszUiState.Done("t", true, null), agentSignRequestFingerprint = "agent",
        rebindState = RebindPublicState.ON_WIFI,
        promptJumpResult = PromptJumpResult(null, 3), promptOutputCopyResult = PromptOutputCopyResult("x", 5),
        aiPanel = panel,
    )

    private fun reduce(s: TerminalUiState, m: UiMsg) = ConnectionStateMapper.reduce(s, m)

    // ── 表テスト(UiMsgごと) ──

    @Test
    fun `ConnectionStateChanged delegates to apply`() {
        val states = listOf(
            ConnectionPublicState.Connecting,
            ConnectionPublicState.Connected("h"),
            ConnectionPublicState.Disconnected("r", null),
            ConnectionPublicState.Error("e"),
            ConnectionPublicState.Reconnecting(1u, 10u, "why"),
        )
        for (st in states) {
            assertEquals(ConnectionStateMapper.apply(busy, st), reduce(busy, UiMsg.ConnectionStateChanged(st)))
        }
    }

    @Test
    fun `ConnectFailed clears isConnecting and shows the error`() {
        assertEquals(busy.copy(isConnecting = false, statusMsg = "エラー: boom"), reduce(busy, UiMsg.ConnectFailed("boom")))
        assertEquals(busy.copy(isConnecting = false, statusMsg = "エラー: 不明なエラー"), reduce(busy, UiMsg.ConnectFailed(null)))
    }

    @Test
    fun `LocalDisconnectRequested only touches connected isConnecting statusMsg`() {
        assertEquals(
            busy.copy(connected = false, isConnecting = false, statusMsg = "切断済み"),
            reduce(busy, UiMsg.LocalDisconnectRequested),
        )
    }

    @Test
    fun `ScreenUpdated sets screenUpdate and scrollbackLen`() {
        val u = screenUpdate(9uL)
        assertEquals(busy.copy(screenUpdate = u, scrollbackLen = 42), reduce(busy, UiMsg.ScreenUpdated(u, 42)))
    }

    @Test
    fun `host key messages set and clear their own field only`() {
        val other = HostKeyChangedWarning("x", 1, "a", "b")
        val otherPrompt = NewHostKeyPrompt("y", 2, "z")
        assertEquals(busy.copy(lastFingerprint = "new-fp"), reduce(busy, UiMsg.HostKeyTrustedNew("new-fp")))
        assertEquals(busy.copy(hostKeyChangedWarning = other), reduce(busy, UiMsg.HostKeyChanged(other)))
        assertEquals(busy.copy(hostKeyChangedWarning = null), reduce(busy, UiMsg.HostKeyChangedWarningCleared))
        assertEquals(busy.copy(newHostKeyPrompt = otherPrompt), reduce(busy, UiMsg.NewHostKeyPromptShown(otherPrompt)))
        assertEquals(busy.copy(newHostKeyPrompt = null), reduce(busy, UiMsg.NewHostKeyPromptCleared))
    }

    @Test
    fun `TrzszStateChanged maps through TrzszStateMapper`() {
        val states = listOf(
            TrzszPublicState.Idle,
            TrzszPublicState.WaitingUser("t1", "upload", "f", 1uL),
            TrzszPublicState.InProgress("t1", "download", "f", 1uL, 2uL),
            TrzszPublicState.Done("t1", false, "m"),
        )
        for (st in states) {
            assertEquals(busy.copy(trzszState = TrzszStateMapper.toUiState(st)), reduce(busy, UiMsg.TrzszStateChanged(st)))
        }
        assertNull(reduce(busy, UiMsg.TrzszStateChanged(TrzszPublicState.Idle)).trzszState)
    }

    @Test
    fun `TrzszCancelled clears trzszState only`() {
        assertEquals(busy.copy(trzszState = null), reduce(busy, UiMsg.TrzszCancelled))
    }

    @Test
    fun `RebindStateChanged sets rebindState`() {
        assertEquals(
            busy.copy(rebindState = RebindPublicState.FAILED_OVER_TO_CELLULAR),
            reduce(busy, UiMsg.RebindStateChanged(RebindPublicState.FAILED_OVER_TO_CELLULAR)),
        )
    }

    @Test
    fun `PromptJumpResolved bumps seq even for repeated null results`() {
        val t = PromptJumpTarget(scrollOffset = 12u, isLive = false)
        assertEquals(busy.copy(promptJumpResult = PromptJumpResult(t, 4)), reduce(busy, UiMsg.PromptJumpResolved(t)))
        val twice = reduce(reduce(busy, UiMsg.PromptJumpResolved(null)), UiMsg.PromptJumpResolved(null))
        assertEquals(PromptJumpResult(null, 5), twice.promptJumpResult)
    }

    @Test
    fun `PromptOutputCopyReady bumps seq`() {
        assertEquals(
            busy.copy(promptOutputCopyResult = PromptOutputCopyResult(null, 6)),
            reduce(busy, UiMsg.PromptOutputCopyReady(null)),
        )
        assertEquals(PromptOutputCopyResult("out", 1), reduce(TerminalUiState(), UiMsg.PromptOutputCopyReady("out")).promptOutputCopyResult)
    }

    @Test
    fun `agent sign messages set and clear the fingerprint`() {
        assertEquals(busy.copy(agentSignRequestFingerprint = "k"), reduce(busy, UiMsg.AgentSignRequested("k")))
        assertEquals(busy.copy(agentSignRequestFingerprint = null), reduce(busy, UiMsg.AgentSignRequestCleared))
    }

    @Test
    fun `AI panel messages set and clear aiPanel`() {
        val form = AiPanelUiState(PanelKind.FORM, "f", "", listOf(PanelField("a", "A", PanelFieldKind.TEXT, emptyList())))
        assertEquals(busy.copy(aiPanel = form), reduce(busy, UiMsg.AiPanelPresented(form)))
        assertEquals(busy.copy(aiPanel = null), reduce(busy, UiMsg.AiPanelDismissed))
    }

    @Test
    fun `reduce is pure - same input gives equal output and does not mutate input`() {
        val before = busy.copy()
        val a = reduce(busy, UiMsg.PromptJumpResolved(null))
        val b = reduce(busy, UiMsg.PromptJumpResolved(null))
        assertEquals(a, b)
        assertEquals(before, busy)
    }

    @Test
    fun `reduceAll of empty sequence is identity`() {
        assertSame(busy, ConnectionStateMapper.reduceAll(busy, emptyList()))
    }

    // ── 性質テスト: 旧`_state.update`22箇所との等価性 ──

    /**
     * Step 8b以前の`TerminalSession.kt`(main 0e9e20c3)の`_state.update { ... }`ラムダを
     * 1つずつ書き写したもの(行番号は旧ファイル)。**本番コードを参照せず**ここで独立に
     * 再現することで、reducerが旧挙動から外れたら検出できるようにする。
     */
    private fun legacyApply(it: TerminalUiState, msg: UiMsg): TerminalUiState = when (msg) {
        // L268 onConnectionStateChanged
        is UiMsg.ConnectionStateChanged -> ConnectionStateMapper.apply(it, msg.state)
        // L471 guardedConnect catch(SshException)
        is UiMsg.ConnectFailed -> it.copy(isConnecting = false, statusMsg = "エラー: ${msg.message ?: "不明なエラー"}")
        // L503 disconnect()
        UiMsg.LocalDisconnectRequested -> it.copy(connected = false, isConnecting = false, statusMsg = "切断済み")
        // L427 screenUpdate消費ループ
        is UiMsg.ScreenUpdated -> it.copy(screenUpdate = msg.update, scrollbackLen = msg.scrollbackLen)
        // L283 onHostKey Trust(isNew)
        is UiMsg.HostKeyTrustedNew -> it.copy(lastFingerprint = msg.fingerprint)
        // L289 onHostKey Changed
        is UiMsg.HostKeyChanged -> it.copy(hostKeyChangedWarning = msg.warning)
        // L609 trustUpdatedHostKey / L616 dismissHostKeyWarning
        UiMsg.HostKeyChangedWarningCleared -> it.copy(hostKeyChangedWarning = null)
        // L294 onHostKey Unconfirmed
        is UiMsg.NewHostKeyPromptShown -> it.copy(newHostKeyPrompt = msg.prompt)
        // L625 trustNewHostKey / L632 dismissNewHostKeyPrompt
        UiMsg.NewHostKeyPromptCleared -> it.copy(newHostKeyPrompt = null)
        // L315 onTrzszStateChanged
        is UiMsg.TrzszStateChanged -> it.copy(trzszState = TrzszStateMapper.toUiState(msg.state))
        // L666 trzszCancel
        UiMsg.TrzszCancelled -> it.copy(trzszState = null)
        // L352 onRebindStateChanged
        is UiMsg.RebindStateChanged -> it.copy(rebindState = msg.state)
        // L370 onPromptJump
        is UiMsg.PromptJumpResolved -> it.copy(promptJumpResult = PromptJumpResult(msg.target, it.promptJumpResult.seq + 1))
        // L374 onPromptOutputCopyReady
        is UiMsg.PromptOutputCopyReady ->
            it.copy(promptOutputCopyResult = PromptOutputCopyResult(msg.text, it.promptOutputCopyResult.seq + 1))
        // L384 onAgentSignRequest
        is UiMsg.AgentSignRequested -> it.copy(agentSignRequestFingerprint = msg.keyFingerprint)
        // L396 onAgentSignRequest finally / L641 respondAgentSignRequest
        UiMsg.AgentSignRequestCleared -> it.copy(agentSignRequestFingerprint = null)
        // L204 maybeApplyPanel
        is UiMsg.AiPanelPresented -> it.copy(aiPanel = msg.panel)
        // L220 dismissAiPanel / L459 guardedConnect
        UiMsg.AiPanelDismissed -> it.copy(aiPanel = null)
    }

    /** UiMsgの種類名(`when`の網羅性により、UiMsgを足すとここがコンパイルエラーになり生成器の更新を強制する)。 */
    private fun kindOf(msg: UiMsg): String = when (msg) {
        is UiMsg.ConnectionStateChanged -> "ConnectionStateChanged"
        is UiMsg.ConnectFailed -> "ConnectFailed"
        UiMsg.LocalDisconnectRequested -> "LocalDisconnectRequested"
        is UiMsg.ScreenUpdated -> "ScreenUpdated"
        is UiMsg.HostKeyTrustedNew -> "HostKeyTrustedNew"
        is UiMsg.HostKeyChanged -> "HostKeyChanged"
        UiMsg.HostKeyChangedWarningCleared -> "HostKeyChangedWarningCleared"
        is UiMsg.NewHostKeyPromptShown -> "NewHostKeyPromptShown"
        UiMsg.NewHostKeyPromptCleared -> "NewHostKeyPromptCleared"
        is UiMsg.TrzszStateChanged -> "TrzszStateChanged"
        UiMsg.TrzszCancelled -> "TrzszCancelled"
        is UiMsg.RebindStateChanged -> "RebindStateChanged"
        is UiMsg.PromptJumpResolved -> "PromptJumpResolved"
        is UiMsg.PromptOutputCopyReady -> "PromptOutputCopyReady"
        is UiMsg.AgentSignRequested -> "AgentSignRequested"
        UiMsg.AgentSignRequestCleared -> "AgentSignRequestCleared"
        is UiMsg.AiPanelPresented -> "AiPanelPresented"
        UiMsg.AiPanelDismissed -> "AiPanelDismissed"
    }

    private val generators: List<(Random) -> UiMsg> = listOf(
        { r ->
            UiMsg.ConnectionStateChanged(
                when (r.nextInt(5)) {
                    0 -> ConnectionPublicState.Connecting
                    1 -> ConnectionPublicState.Connected("h${r.nextInt(3)}")
                    2 -> ConnectionPublicState.Disconnected(if (r.nextBoolean()) "r" else null, null)
                    3 -> ConnectionPublicState.Error("e${r.nextInt(3)}")
                    else -> ConnectionPublicState.Reconnecting(r.nextInt(10).toUInt(), 30u, if (r.nextBoolean()) "x" else null)
                },
            )
        },
        { r -> UiMsg.ConnectFailed(if (r.nextBoolean()) "boom" else null) },
        { _ -> UiMsg.LocalDisconnectRequested },
        { r -> UiMsg.ScreenUpdated(screenUpdate(r.nextInt(100).toULong()), r.nextInt(1000)) },
        { r -> UiMsg.HostKeyTrustedNew("fp${r.nextInt(5)}") },
        { r -> UiMsg.HostKeyChanged(HostKeyChangedWarning("h", r.nextInt(3), "o", "n")) },
        { _ -> UiMsg.HostKeyChangedWarningCleared },
        { r -> UiMsg.NewHostKeyPromptShown(NewHostKeyPrompt("h", r.nextInt(3), "fp")) },
        { _ -> UiMsg.NewHostKeyPromptCleared },
        { r ->
            UiMsg.TrzszStateChanged(
                when (r.nextInt(4)) {
                    0 -> TrzszPublicState.Idle
                    1 -> TrzszPublicState.WaitingUser("t", "upload", null, null)
                    2 -> TrzszPublicState.InProgress("t", "download", "f", r.nextInt(10).toULong(), 10uL)
                    else -> TrzszPublicState.Done("t", r.nextBoolean(), null)
                },
            )
        },
        { _ -> UiMsg.TrzszCancelled },
        { r -> UiMsg.RebindStateChanged(RebindPublicState.entries[r.nextInt(RebindPublicState.entries.size)]) },
        { r -> UiMsg.PromptJumpResolved(if (r.nextBoolean()) PromptJumpTarget(r.nextInt(50).toUInt(), r.nextBoolean()) else null) },
        { r -> UiMsg.PromptOutputCopyReady(if (r.nextBoolean()) "out${r.nextInt(3)}" else null) },
        { r -> UiMsg.AgentSignRequested("k${r.nextInt(3)}") },
        { _ -> UiMsg.AgentSignRequestCleared },
        { r -> UiMsg.AiPanelPresented(AiPanelUiState(PanelKind.DOCUMENT, "t${r.nextInt(3)}", "m", emptyList())) },
        { _ -> UiMsg.AiPanelDismissed },
    )

    @Test
    fun `generators cover every UiMsg kind`() {
        val r = Random(0)
        val kinds = generators.map { kindOf(it(r)) }.toSet()
        // kindOfの分岐数(=UiMsgの種類数)と一致すること。
        assertEquals(18, kinds.size)
        assertEquals(generators.size, kinds.size)
    }

    @Test
    fun `folding any message sequence equals folding the old per-callback copies`() {
        for (seed in 0 until 500) {
            val r = Random(seed)
            val msgs = List(r.nextInt(0, 60)) { generators[r.nextInt(generators.size)](r) }
            val initial = if (r.nextBoolean()) TerminalUiState() else busy
            var expected = initial
            var actual = initial
            for ((i, m) in msgs.withIndex()) {
                expected = legacyApply(expected, m)
                actual = reduce(actual, m)
                assertEquals("seed=$seed step=$i msg=$m", expected, actual)
            }
            assertEquals("seed=$seed reduceAll", expected, ConnectionStateMapper.reduceAll(initial, msgs))
        }
    }

    @Test
    fun `seq counters equal the number of corresponding messages`() {
        val r = Random(42)
        val msgs = List(300) { generators[r.nextInt(generators.size)](r) }
        val end = ConnectionStateMapper.reduceAll(TerminalUiState(), msgs)
        assertEquals(msgs.count { it is UiMsg.PromptJumpResolved }.toLong(), end.promptJumpResult.seq)
        assertEquals(msgs.count { it is UiMsg.PromptOutputCopyReady }.toLong(), end.promptOutputCopyResult.seq)
        assertTrue(end.promptJumpResult.seq > 0)
    }
}
