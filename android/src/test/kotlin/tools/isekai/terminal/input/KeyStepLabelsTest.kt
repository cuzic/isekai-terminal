package tools.isekai.terminal.input

import org.junit.Assert.*
import org.junit.Test

class KeyStepLabelsTest {

    @Test
    fun `CtrlChar shows caret notation`() {
        assertEquals("^B", KeyStep.CtrlChar('b').shortLabel())
        assertEquals("^B", KeyStep.CtrlChar('B').shortLabel())
    }

    @Test
    fun `Text shows literal text`() {
        assertEquals("c", KeyStep.Text("c").shortLabel())
    }

    @Test
    fun `Special shows friendly name for known keys`() {
        assertEquals("Enter", KeyStep.Special(TerminalKeyEncoder.KC_ENTER).shortLabel())
        assertEquals("↑", KeyStep.Special(TerminalKeyEncoder.KC_DPAD_UP).shortLabel())
        assertEquals("F5", KeyStep.Special(TerminalKeyEncoder.KC_F5).shortLabel())
    }

    @Test
    fun `Special shows fallback for unknown keyCode`() {
        assertEquals("Key(9999)", KeyStep.Special(9999).shortLabel())
    }

    @Test
    fun `PlaceholderRef shows braces`() {
        assertEquals("{prefix}", KeyStep.PlaceholderRef("prefix").shortLabel())
    }

    @Test
    fun `previewText joins steps with spaces - tmux new window`() {
        val steps = listOf(KeyStep.PlaceholderRef("prefix"), KeyStep.Text("c"))
        assertEquals("{prefix} c", steps.previewText())
    }

    @Test
    fun `previewText of empty list is empty string`() {
        assertEquals("", emptyList<KeyStep>().previewText())
    }
}
