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
}
