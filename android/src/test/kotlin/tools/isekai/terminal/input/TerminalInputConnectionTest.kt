package tools.isekai.terminal.input

import android.app.Application
import android.view.KeyEvent
import androidx.test.core.app.ApplicationProvider
import org.junit.After
import org.junit.Assert.assertArrayEquals
import org.junit.Assert.assertEquals
import org.junit.Assert.assertTrue
import org.junit.Before
import org.junit.Test
import org.junit.runner.RunWith
import org.robolectric.RobolectricTestRunner
import org.robolectric.annotation.Config

@RunWith(RobolectricTestRunner::class)
@Config(sdk = [28])
class TerminalInputConnectionTest {

    private lateinit var view: TerminalInputView
    private lateinit var connection: TerminalInputConnection
    private val sentBytes = mutableListOf<ByteArray>()
    private val composingTexts = mutableListOf<String>()

    @Before
    fun setup() {
        val context = ApplicationProvider.getApplicationContext<Application>()
        view = TerminalInputView(context)
        view.onSendBytes = { bytes -> sentBytes.add(bytes) }
        view.onComposingText = { text -> composingTexts.add(text) }
        connection = TerminalInputConnection(view)
    }

    @After
    fun teardown() {
        sentBytes.clear()
        composingTexts.clear()
    }

    // --- commitText ---

    @Test
    fun commitText_singleChar_sentRaw() {
        connection.commitText("a", 1)
        assertEquals(1, sentBytes.size)
        assertArrayEquals("a".toByteArray(Charsets.UTF_8), sentBytes[0])
    }

    @Test
    fun commitText_singleKanji_sentRaw() {
        connection.commitText("あ", 1)
        assertEquals(1, sentBytes.size)
        assertArrayEquals("あ".toByteArray(Charsets.UTF_8), sentBytes[0])
    }

    @Test
    fun commitText_multiChar_bracketedPaste() {
        view.bracketedPasteMode = true
        connection.commitText("hello", 1)
        assertEquals(1, sentBytes.size)
        val bytes = sentBytes[0]
        assertArrayEquals(
            byteArrayOf(0x1B, '['.code.toByte(), '2'.code.toByte(), '0'.code.toByte(), '0'.code.toByte(), '~'.code.toByte()),
            bytes.copyOfRange(0, 6),
        )
        assertArrayEquals(
            byteArrayOf(0x1B, '['.code.toByte(), '2'.code.toByte(), '0'.code.toByte(), '1'.code.toByte(), '~'.code.toByte()),
            bytes.copyOfRange(bytes.size - 6, bytes.size),
        )
        assertTrue(String(bytes, Charsets.UTF_8).contains("hello"))
    }

    @Test
    fun commitText_emptyString_nothingSent() {
        connection.commitText("", 1)
        assertTrue(sentBytes.isEmpty())
    }

    @Test
    fun commitText_clearsComposingCallback() {
        connection.setComposingText("abc", 1)
        connection.commitText("abc", 1)
        assertEquals("", composingTexts.last())
    }

    // --- setComposingText ---

    @Test
    fun setComposingText_firesCallback() {
        connection.setComposingText("テスト", 1)
        assertTrue(composingTexts.contains("テスト"))
    }

    @Test
    fun setComposingText_empty_firesEmptyCallback() {
        connection.setComposingText("", 1)
        assertEquals("", composingTexts.last())
    }

    @Test
    fun setComposingText_update_firesUpdatedText() {
        connection.setComposingText("ab", 1)
        connection.setComposingText("abc", 1)
        assertTrue(composingTexts.contains("ab"))
        assertTrue(composingTexts.contains("abc"))
    }

    // --- finishComposingText ---

    @Test
    fun finishComposingText_withPending_sendsPending() {
        connection.setComposingText("xyz", 1)
        connection.finishComposingText()
        assertTrue(sentBytes.any { it.contentEquals("xyz".toByteArray(Charsets.UTF_8)) })
    }

    @Test
    fun finishComposingText_noPending_sendsNothing() {
        connection.finishComposingText()
        assertTrue(sentBytes.isEmpty())
    }

    @Test
    fun finishComposingText_clearsComposing() {
        connection.setComposingText("ab", 1)
        connection.finishComposingText()
        assertEquals("", composingTexts.last())
    }

    // --- deleteSurroundingText ---

    @Test
    fun deleteSurroundingText_withComposing_shortensBuffer() {
        connection.setComposingText("abc", 1)
        connection.deleteSurroundingText(1, 0)
        assertEquals("ab", composingTexts.last())
    }

    @Test
    fun deleteSurroundingText_withComposing_deleteAll_sendsEmpty() {
        connection.setComposingText("a", 1)
        connection.deleteSurroundingText(1, 0)
        assertEquals("", composingTexts.last())
    }

    @Test
    fun deleteSurroundingText_noComposing_sendsDEL() {
        connection.deleteSurroundingText(1, 0)
        assertEquals(1, sentBytes.size)
        assertArrayEquals(byteArrayOf(0x7F), sentBytes[0])
    }

    @Test
    fun deleteSurroundingText_noComposing_multiple_sendsMultipleDEL() {
        connection.deleteSurroundingText(3, 0)
        assertEquals(3, sentBytes.size)
        sentBytes.forEach { assertArrayEquals(byteArrayOf(0x7F), it) }
    }

    // --- sendKeyEvent ---

    private fun keyDown(keyCode: Int) {
        connection.sendKeyEvent(KeyEvent(KeyEvent.ACTION_DOWN, keyCode))
    }

    private fun keyDownMeta(keyCode: Int, metaState: Int) {
        connection.sendKeyEvent(KeyEvent(0L, 0L, KeyEvent.ACTION_DOWN, keyCode, 0, metaState))
    }

    @Test
    fun sendKeyEvent_enter_sendsCarriageReturn() {
        keyDown(KeyEvent.KEYCODE_ENTER)
        assertArrayEquals(byteArrayOf(0x0D), sentBytes[0])
    }

    @Test
    fun sendKeyEvent_del_sendsBackspace() {
        keyDown(KeyEvent.KEYCODE_DEL)
        assertArrayEquals(byteArrayOf(0x7F), sentBytes[0])
    }

    @Test
    fun sendKeyEvent_tab_sendsTab() {
        keyDown(KeyEvent.KEYCODE_TAB)
        assertArrayEquals(byteArrayOf(0x09), sentBytes[0])
    }

    @Test
    fun sendKeyEvent_escape_sendsEscape() {
        keyDown(KeyEvent.KEYCODE_ESCAPE)
        assertArrayEquals(byteArrayOf(0x1B), sentBytes[0])
    }

    // タスク#72: Kitty keyboard protocol(タスク#54)のdisambiguate escape codes(bit0)が
    // negotiateされている場合、物理キーボードのEscapeキーもCSI u形式で送られることを
    // end-to-end(view.kittyKeyboardFlags → TerminalInputConnection → TerminalKeyEncoder)
    // で確認する。エンコード自体のgolden testは`TerminalKeyEncoderTest`に既にある。
    @Test
    fun sendKeyEvent_escape_usesKittyCsiUWhenDisambiguateFlagNegotiated() {
        view.kittyKeyboardFlags = 0b1u
        keyDown(KeyEvent.KEYCODE_ESCAPE)
        assertArrayEquals(byteArrayOf(0x1B, 0x5B, 0x32, 0x37, 0x75), sentBytes[0]) // ESC[27u
    }

    @Test
    fun sendKeyEvent_arrowUp_sendsCsiA() {
        keyDown(KeyEvent.KEYCODE_DPAD_UP)
        assertArrayEquals(byteArrayOf(0x1B, 0x5B, 0x41), sentBytes[0])
    }

    @Test
    fun sendKeyEvent_arrowDown_sendsCsiB() {
        keyDown(KeyEvent.KEYCODE_DPAD_DOWN)
        assertArrayEquals(byteArrayOf(0x1B, 0x5B, 0x42), sentBytes[0])
    }

    @Test
    fun sendKeyEvent_arrowRight_sendsCsiC() {
        keyDown(KeyEvent.KEYCODE_DPAD_RIGHT)
        assertArrayEquals(byteArrayOf(0x1B, 0x5B, 0x43), sentBytes[0])
    }

    @Test
    fun sendKeyEvent_arrowLeft_sendsCsiD() {
        keyDown(KeyEvent.KEYCODE_DPAD_LEFT)
        assertArrayEquals(byteArrayOf(0x1B, 0x5B, 0x44), sentBytes[0])
    }

    @Test
    fun sendKeyEvent_pageUp_sendsCsi5Tilde() {
        keyDown(KeyEvent.KEYCODE_PAGE_UP)
        assertArrayEquals(byteArrayOf(0x1B, 0x5B, 0x35, 0x7E), sentBytes[0])
    }

    @Test
    fun sendKeyEvent_pageDown_sendsCsi6Tilde() {
        keyDown(KeyEvent.KEYCODE_PAGE_DOWN)
        assertArrayEquals(byteArrayOf(0x1B, 0x5B, 0x36, 0x7E), sentBytes[0])
    }

    @Test
    fun sendKeyEvent_home_sendsCsiH() {
        keyDown(KeyEvent.KEYCODE_MOVE_HOME)
        assertArrayEquals(byteArrayOf(0x1B, 0x5B, 0x48), sentBytes[0])
    }

    @Test
    fun sendKeyEvent_end_sendsCsiF() {
        keyDown(KeyEvent.KEYCODE_MOVE_END)
        assertArrayEquals(byteArrayOf(0x1B, 0x5B, 0x46), sentBytes[0])
    }

    @Test
    fun sendKeyEvent_actionUp_ignored() {
        connection.sendKeyEvent(KeyEvent(KeyEvent.ACTION_UP, KeyEvent.KEYCODE_ENTER))
        assertTrue(sentBytes.isEmpty())
    }

    // --- ctrlArmed (トグル式 Ctrl キー) ---

    @Test
    fun commitText_ctrlArmed_lowerA_sendsCtrlA_andDisarms() {
        view.ctrlArmed = true
        var consumed = false
        view.onCtrlConsumed = { consumed = true }
        connection.commitText("a", 1)
        assertEquals(1, sentBytes.size)
        assertArrayEquals(byteArrayOf(0x01), sentBytes[0])
        assertTrue(consumed)
        assertEquals(false, view.ctrlArmed)
    }

    @Test
    fun commitText_ctrlArmed_japanese_fallsThroughRaw_andDisarms() {
        view.ctrlArmed = true
        var consumed = false
        view.onCtrlConsumed = { consumed = true }
        connection.commitText("あ", 1)
        assertEquals(1, sentBytes.size)
        assertArrayEquals("あ".toByteArray(Charsets.UTF_8), sentBytes[0])
        assertTrue(consumed)
        assertEquals(false, view.ctrlArmed)
    }

    @Test
    fun commitText_ctrlNotArmed_plainCharSentRaw() {
        connection.commitText("a", 1)
        assertEquals(1, sentBytes.size)
        assertArrayEquals("a".toByteArray(Charsets.UTF_8), sentBytes[0])
    }

    @Test
    fun sendKeyEvent_ctrlArmed_physicalCtrlPressed_notConsumed() {
        view.ctrlArmed = true
        var consumed = false
        view.onCtrlConsumed = { consumed = true }
        val event = KeyEvent(
            0L, 0L, KeyEvent.ACTION_DOWN, KeyEvent.KEYCODE_A, 0, KeyEvent.META_CTRL_ON,
        )
        connection.sendKeyEvent(event)
        // 物理 Ctrl 併用時はトグルを消費せず素通し（二重変換防止）
        assertEquals(true, view.ctrlArmed)
        assertEquals(false, consumed)
    }

    @Test
    fun sendKeyEvent_ctrlArmed_plainKey_sendsCtrlByte_andDisarms() {
        view.ctrlArmed = true
        var consumed = false
        view.onCtrlConsumed = { consumed = true }
        keyDown(KeyEvent.KEYCODE_A)
        assertArrayEquals(byteArrayOf(0x01), sentBytes[0])
        assertTrue(consumed)
        assertEquals(false, view.ctrlArmed)
    }

    // --- JIS配列固有キー(¥/ろ) ---

    private fun keyDownWithShift(keyCode: Int, shiftPressed: Boolean = false): Boolean {
        val metaState = if (shiftPressed) KeyEvent.META_SHIFT_ON else 0
        return connection.sendKeyEvent(
            KeyEvent(0L, 0L, KeyEvent.ACTION_DOWN, keyCode, 0, metaState),
        )
    }

    @Test
    fun sendKeyEvent_yenKey_jisMode_unshifted_sendsBackslash() {
        view.keyboardLayoutMode = KeyboardLayoutMode.JIS
        keyDownWithShift(TerminalKeyEncoder.KC_YEN)
        assertArrayEquals(byteArrayOf(0x5C), sentBytes[0])
    }

    @Test
    fun sendKeyEvent_yenKey_jisMode_shifted_sendsPipe() {
        view.keyboardLayoutMode = KeyboardLayoutMode.JIS
        keyDownWithShift(TerminalKeyEncoder.KC_YEN, shiftPressed = true)
        assertArrayEquals(byteArrayOf(0x7C), sentBytes[0])
    }

    @Test
    fun sendKeyEvent_roKey_jisMode_unshifted_sendsBackslash() {
        view.keyboardLayoutMode = KeyboardLayoutMode.JIS
        keyDownWithShift(TerminalKeyEncoder.KC_RO)
        assertArrayEquals(byteArrayOf(0x5C), sentBytes[0])
    }

    @Test
    fun sendKeyEvent_roKey_jisMode_shifted_sendsUnderscore() {
        view.keyboardLayoutMode = KeyboardLayoutMode.JIS
        keyDownWithShift(TerminalKeyEncoder.KC_RO, shiftPressed = true)
        assertArrayEquals(byteArrayOf(0x5F), sentBytes[0])
    }

    @Test
    fun sendKeyEvent_yenKey_usMode_fallsThroughWithoutSending() {
        view.keyboardLayoutMode = KeyboardLayoutMode.US
        keyDownWithShift(TerminalKeyEncoder.KC_YEN)
        // US配列モードでは明示マッピングを行わない。getUnicodeChar()も定義が無いため
        // 何も送信されない(実機のUS配列キーボードにこのキー自体が存在しないのと同じ挙動)。
        assertTrue(sentBytes.isEmpty())
    }

    @Test
    fun sendKeyEvent_yenKey_autoMode_withoutDevice_defaultsToUsBehavior() {
        // Robolectric上の合成KeyEvent(deviceId未指定)にはInputDeviceが紐付かないため、
        // AUTO判定はJISキーボード「無し」に倒れる(実機での自動検出そのものはロボレクトリック環境では
        // 検証できない。KeyboardLayoutDetectorTest参照)。
        view.keyboardLayoutMode = KeyboardLayoutMode.AUTO
        keyDownWithShift(TerminalKeyEncoder.KC_YEN)
        assertTrue(sentBytes.isEmpty())
    }

    // --- 物理 Ctrl/Alt 修飾キー（トグルではなく実キーボードの修飾キー） ---

    @Test
    fun sendKeyEvent_physicalCtrlPlusA_sendsCtrlByte() {
        keyDownMeta(KeyEvent.KEYCODE_A, KeyEvent.META_CTRL_ON)
        assertArrayEquals(byteArrayOf(0x01), sentBytes[0])
    }

    @Test
    fun sendKeyEvent_physicalCtrlPlusA_doesNotArmOrConsumeToggle() {
        var consumed = false
        view.onCtrlConsumed = { consumed = true }
        keyDownMeta(KeyEvent.KEYCODE_A, KeyEvent.META_CTRL_ON)
        assertEquals(false, view.ctrlArmed)
        assertEquals(false, consumed)
    }

    @Test
    fun sendKeyEvent_physicalAltPlusB_sendsEscPrefixedByte() {
        keyDownMeta(KeyEvent.KEYCODE_B, KeyEvent.META_ALT_ON)
        assertArrayEquals(byteArrayOf(0x1B) + "b".toByteArray(Charsets.UTF_8), sentBytes[0])
    }

    // --- コピー / 貼り付けショートカット ---

    @Test
    fun sendKeyEvent_ctrlShiftC_invokesOnCopyRequested() {
        var called = false
        view.onCopyRequested = { called = true }
        keyDownMeta(KeyEvent.KEYCODE_C, KeyEvent.META_CTRL_ON or KeyEvent.META_SHIFT_ON)
        assertTrue(called)
        assertTrue(sentBytes.isEmpty())
    }

    @Test
    fun sendKeyEvent_ctrlShiftV_invokesOnPasteRequested() {
        var called = false
        view.onPasteRequested = { called = true }
        keyDownMeta(KeyEvent.KEYCODE_V, KeyEvent.META_CTRL_ON or KeyEvent.META_SHIFT_ON)
        assertTrue(called)
        assertTrue(sentBytes.isEmpty())
    }

    @Test
    fun sendKeyEvent_metaC_invokesOnCopyRequested() {
        var called = false
        view.onCopyRequested = { called = true }
        keyDownMeta(KeyEvent.KEYCODE_C, KeyEvent.META_META_ON)
        assertTrue(called)
    }

    @Test
    fun sendKeyEvent_metaV_invokesOnPasteRequested() {
        var called = false
        view.onPasteRequested = { called = true }
        keyDownMeta(KeyEvent.KEYCODE_V, KeyEvent.META_META_ON)
        assertTrue(called)
    }

    @Test
    fun sendKeyEvent_hardwareCopyKey_invokesOnCopyRequested() {
        var called = false
        view.onCopyRequested = { called = true }
        keyDown(KeyEvent.KEYCODE_COPY)
        assertTrue(called)
    }

    @Test
    fun sendKeyEvent_hardwarePasteKey_invokesOnPasteRequested() {
        var called = false
        view.onPasteRequested = { called = true }
        keyDown(KeyEvent.KEYCODE_PASTE)
        assertTrue(called)
    }

    @Test
    fun sendKeyEvent_ctrlShiftC_noCopyCallbackWired_fallsBackToCtrlC() {
        // onCopyRequested 未配線: アプリショートカットとして扱われず、物理 Ctrl+文字 の
        // 一般変換にフォールスルーする（Shift の有無は ctrlByte 側では区別しない）。
        keyDownMeta(KeyEvent.KEYCODE_C, KeyEvent.META_CTRL_ON or KeyEvent.META_SHIFT_ON)
        assertArrayEquals(byteArrayOf(0x03), sentBytes[0])
    }

    // --- タブ切替ショートカット ---

    @Test
    fun sendKeyEvent_ctrlTab_invokesOnNextTabRequested_andDoesNotSendTabByte() {
        var called = false
        view.onNextTabRequested = { called = true }
        keyDownMeta(KeyEvent.KEYCODE_TAB, KeyEvent.META_CTRL_ON)
        assertTrue(called)
        assertTrue(sentBytes.isEmpty())
    }

    @Test
    fun sendKeyEvent_ctrlShiftTab_invokesOnPreviousTabRequested_andDoesNotSendTabByte() {
        var called = false
        view.onPreviousTabRequested = { called = true }
        keyDownMeta(KeyEvent.KEYCODE_TAB, KeyEvent.META_CTRL_ON or KeyEvent.META_SHIFT_ON)
        assertTrue(called)
        assertTrue(sentBytes.isEmpty())
    }

    @Test
    fun sendKeyEvent_ctrlTab_noCallbackWired_fallsBackToTabByte() {
        keyDownMeta(KeyEvent.KEYCODE_TAB, KeyEvent.META_CTRL_ON)
        assertArrayEquals(byteArrayOf(0x09), sentBytes[0])
    }

    // --- IME 変換中はショートカットを誤発火させない（composingText 状態を参照） ---

    @Test
    fun sendKeyEvent_composing_physicalCtrlA_doesNotSendCtrlByte() {
        connection.setComposingText("あ", 1)
        sentBytes.clear()
        keyDownMeta(KeyEvent.KEYCODE_A, KeyEvent.META_CTRL_ON)
        assertTrue(sentBytes.none { it.contentEquals(byteArrayOf(0x01)) })
    }

    @Test
    fun sendKeyEvent_composing_physicalAltB_doesNotSendEscPrefixedByte() {
        connection.setComposingText("あ", 1)
        sentBytes.clear()
        val escB = byteArrayOf(0x1B) + "b".toByteArray(Charsets.UTF_8)
        keyDownMeta(KeyEvent.KEYCODE_B, KeyEvent.META_ALT_ON)
        assertTrue(sentBytes.none { it.contentEquals(escB) })
    }

    @Test
    fun sendKeyEvent_composing_ctrlShiftC_doesNotInvokeOnCopyRequested() {
        var called = false
        view.onCopyRequested = { called = true }
        connection.setComposingText("あ", 1)
        keyDownMeta(KeyEvent.KEYCODE_C, KeyEvent.META_CTRL_ON or KeyEvent.META_SHIFT_ON)
        assertEquals(false, called)
    }

    @Test
    fun sendKeyEvent_composing_ctrlTab_doesNotInvokeOnNextTabRequested() {
        var called = false
        view.onNextTabRequested = { called = true }
        connection.setComposingText("あ", 1)
        keyDownMeta(KeyEvent.KEYCODE_TAB, KeyEvent.META_CTRL_ON)
        assertEquals(false, called)
    }

    @Test
    fun sendKeyEvent_afterComposingFinished_physicalCtrlA_sendsCtrlByteAgain() {
        connection.setComposingText("あ", 1)
        connection.finishComposingText()
        sentBytes.clear()
        keyDownMeta(KeyEvent.KEYCODE_A, KeyEvent.META_CTRL_ON)
        assertArrayEquals(byteArrayOf(0x01), sentBytes[0])
    }

    // --- 物理修飾キー付き特殊キー(タスク#30、Codexレビュー指摘: Ctrl+矢印が通常矢印として
    //     扱われてしまうバグの修正確認) ---

    @Test
    fun sendKeyEvent_physicalCtrlPlusArrowUp_sendsModifiedCsiSequence() {
        keyDownMeta(KeyEvent.KEYCODE_DPAD_UP, KeyEvent.META_CTRL_ON)
        // ESC[1;5A（xterm互換のCtrl修飾子付きCSI、rust-core `terminal_special_key_bytes`(#29)と同一）
        assertArrayEquals(byteArrayOf(0x1B, 0x5B, 0x31, 0x3B, 0x35, 0x41), sentBytes[0])
    }

    @Test
    fun sendKeyEvent_physicalCtrlPlusArrowUp_ignoresApplicationCursorMode() {
        view.applicationCursorMode = true
        keyDownMeta(KeyEvent.KEYCODE_DPAD_UP, KeyEvent.META_CTRL_ON)
        // 修飾子付きは常にCSI形式(DECCKMが有効でもSS3にはならない)
        assertArrayEquals(byteArrayOf(0x1B, 0x5B, 0x31, 0x3B, 0x35, 0x41), sentBytes[0])
    }

    @Test
    fun sendKeyEvent_physicalShiftPlusTab_sendsCbt() {
        keyDownMeta(KeyEvent.KEYCODE_TAB, KeyEvent.META_SHIFT_ON)
        assertArrayEquals(byteArrayOf(0x1B, 0x5B, 0x5A), sentBytes[0])
    }

    @Test
    fun sendKeyEvent_plainArrowUp_stillUnaffectedByModifierChange() {
        // 修飾なしの場合の既存挙動(applicationCursorMode=false → CSI)に回帰が無いことを確認
        keyDown(KeyEvent.KEYCODE_DPAD_UP)
        assertArrayEquals(byteArrayOf(0x1B, 0x5B, 0x41), sentBytes[0])
    }

    // --- テンキー NumLock 状態(タスク#83、fableレビュー指摘: event.isNumLockOnが未使用だった) ---

    private fun keyDownNumLock(keyCode: Int, numLockOn: Boolean): Boolean {
        val metaState = if (numLockOn) KeyEvent.META_NUM_LOCK_ON else 0
        return connection.sendKeyEvent(
            KeyEvent(0L, 0L, KeyEvent.ACTION_DOWN, keyCode, 0, metaState),
        )
    }

    @Test
    fun sendKeyEvent_numpad8_numLockOn_sendsLiteralDigit() {
        keyDownNumLock(TerminalKeyEncoder.KC_NUMPAD_8, numLockOn = true)
        assertArrayEquals(byteArrayOf('8'.code.toByte()), sentBytes[0])
    }

    @Test
    fun sendKeyEvent_numpad8_numLockOff_sendsArrowUp() {
        keyDownNumLock(TerminalKeyEncoder.KC_NUMPAD_8, numLockOn = false)
        assertArrayEquals(byteArrayOf(0x1B, 0x5B, 0x41), sentBytes[0])
    }

    @Test
    fun sendKeyEvent_numpad2_numLockOff_sendsArrowDown() {
        keyDownNumLock(TerminalKeyEncoder.KC_NUMPAD_2, numLockOn = false)
        assertArrayEquals(byteArrayOf(0x1B, 0x5B, 0x42), sentBytes[0])
    }

    @Test
    fun sendKeyEvent_numpad6_numLockOff_sendsArrowRight() {
        keyDownNumLock(TerminalKeyEncoder.KC_NUMPAD_6, numLockOn = false)
        assertArrayEquals(byteArrayOf(0x1B, 0x5B, 0x43), sentBytes[0])
    }

    @Test
    fun sendKeyEvent_numpad4_numLockOff_sendsArrowLeft() {
        keyDownNumLock(TerminalKeyEncoder.KC_NUMPAD_4, numLockOn = false)
        assertArrayEquals(byteArrayOf(0x1B, 0x5B, 0x44), sentBytes[0])
    }

    @Test
    fun sendKeyEvent_numpad7_numLockOff_sendsHome() {
        keyDownNumLock(TerminalKeyEncoder.KC_NUMPAD_7, numLockOn = false)
        assertArrayEquals(byteArrayOf(0x1B, 0x5B, 0x48), sentBytes[0])
    }

    @Test
    fun sendKeyEvent_numpad1_numLockOff_sendsEnd() {
        keyDownNumLock(TerminalKeyEncoder.KC_NUMPAD_1, numLockOn = false)
        assertArrayEquals(byteArrayOf(0x1B, 0x5B, 0x46), sentBytes[0])
    }

    @Test
    fun sendKeyEvent_numpad9_numLockOff_sendsPageUp() {
        keyDownNumLock(TerminalKeyEncoder.KC_NUMPAD_9, numLockOn = false)
        assertArrayEquals(byteArrayOf(0x1B, 0x5B, 0x35, 0x7E), sentBytes[0])
    }

    @Test
    fun sendKeyEvent_numpad3_numLockOff_sendsPageDown() {
        keyDownNumLock(TerminalKeyEncoder.KC_NUMPAD_3, numLockOn = false)
        assertArrayEquals(byteArrayOf(0x1B, 0x5B, 0x36, 0x7E), sentBytes[0])
    }

    @Test
    fun sendKeyEvent_numpad0_numLockOff_sendsInsert() {
        keyDownNumLock(TerminalKeyEncoder.KC_NUMPAD_0, numLockOn = false)
        assertArrayEquals(byteArrayOf(0x1B, 0x5B, 0x32, 0x7E), sentBytes[0]) // ESC[2~
    }

    @Test
    fun sendKeyEvent_numpad0_numLockOn_sendsLiteralDigit() {
        keyDownNumLock(TerminalKeyEncoder.KC_NUMPAD_0, numLockOn = true)
        assertArrayEquals(byteArrayOf('0'.code.toByte()), sentBytes[0])
    }

    @Test
    fun sendKeyEvent_numpadDot_numLockOff_sendsForwardDelete() {
        keyDownNumLock(TerminalKeyEncoder.KC_NUMPAD_DOT, numLockOn = false)
        assertArrayEquals(byteArrayOf(0x1B, 0x5B, 0x33, 0x7E), sentBytes[0]) // ESC[3~
    }

    @Test
    fun sendKeyEvent_numpadDot_numLockOn_sendsLiteralDot() {
        keyDownNumLock(TerminalKeyEncoder.KC_NUMPAD_DOT, numLockOn = true)
        assertArrayEquals(byteArrayOf('.'.code.toByte()), sentBytes[0])
    }

    @Test
    fun sendKeyEvent_numpad5_numLockOff_unaffected_sendsLiteralDigit() {
        // 中央キー(5)にはナビゲーション相当が無いため、NumLockに関わらず従来通りリテラル文字を送る
        keyDownNumLock(TerminalKeyEncoder.KC_NUMPAD_5, numLockOn = false)
        assertArrayEquals(byteArrayOf('5'.code.toByte()), sentBytes[0])
    }

    @Test
    fun sendKeyEvent_numpadAdd_numLockOff_unaffected_sendsLiteralPlus() {
        // 四則演算子はNumLockの影響を受けない(実キーボードの慣習)
        keyDownNumLock(TerminalKeyEncoder.KC_NUMPAD_ADD, numLockOn = false)
        assertArrayEquals(byteArrayOf('+'.code.toByte()), sentBytes[0])
    }

    @Test
    fun sendKeyEvent_numpad8_numLockOff_respectsApplicationCursorMode() {
        // NumLock OFF → 矢印キー相当への変換後も、既存の矢印キーと同じくDECCKMを尊重する
        view.applicationCursorMode = true
        keyDownNumLock(TerminalKeyEncoder.KC_NUMPAD_8, numLockOn = false)
        assertArrayEquals(byteArrayOf(0x1B, 0x4F, 0x41), sentBytes[0])
    }
}
