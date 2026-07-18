package tools.isekai.terminal

import tools.isekai.terminal.input.TerminalKeyEncoder
import org.junit.Assert.*
import org.junit.Test
import uniffi.isekai_terminal_core.TerminalKeyModifiers

class TerminalKeyEncoderTest {

    // ── specialKeyBytes ────────────────────────────────────────────

    @Test
    fun `Enter maps to CR 0x0D`() {
        assertArrayEquals(byteArrayOf(0x0D), TerminalKeyEncoder.specialKeyBytes(TerminalKeyEncoder.KC_ENTER))
    }

    @Test
    fun `Del maps to 0x7F`() {
        assertArrayEquals(byteArrayOf(0x7F), TerminalKeyEncoder.specialKeyBytes(TerminalKeyEncoder.KC_DEL))
    }

    @Test
    fun `Tab maps to 0x09`() {
        assertArrayEquals(byteArrayOf(0x09), TerminalKeyEncoder.specialKeyBytes(TerminalKeyEncoder.KC_TAB))
    }

    @Test
    fun `Escape maps to 0x1B`() {
        assertArrayEquals(byteArrayOf(0x1B), TerminalKeyEncoder.specialKeyBytes(TerminalKeyEncoder.KC_ESCAPE))
    }

    @Test
    fun `arrow up maps to CSI A`() {
        assertArrayEquals(byteArrayOf(0x1B, 0x5B, 0x41), TerminalKeyEncoder.specialKeyBytes(TerminalKeyEncoder.KC_DPAD_UP))
    }

    @Test
    fun `arrow down maps to CSI B`() {
        assertArrayEquals(byteArrayOf(0x1B, 0x5B, 0x42), TerminalKeyEncoder.specialKeyBytes(TerminalKeyEncoder.KC_DPAD_DOWN))
    }

    @Test
    fun `arrow right maps to CSI C`() {
        assertArrayEquals(byteArrayOf(0x1B, 0x5B, 0x43), TerminalKeyEncoder.specialKeyBytes(TerminalKeyEncoder.KC_DPAD_RIGHT))
    }

    @Test
    fun `arrow left maps to CSI D`() {
        assertArrayEquals(byteArrayOf(0x1B, 0x5B, 0x44), TerminalKeyEncoder.specialKeyBytes(TerminalKeyEncoder.KC_DPAD_LEFT))
    }

    @Test
    fun `DECCKM arrow up maps to SS3 A`() {
        assertArrayEquals(byteArrayOf(0x1B, 0x4F, 0x41), TerminalKeyEncoder.specialKeyBytes(TerminalKeyEncoder.KC_DPAD_UP, applicationCursorMode = true))
    }

    @Test
    fun `DECCKM arrow down maps to SS3 B`() {
        assertArrayEquals(byteArrayOf(0x1B, 0x4F, 0x42), TerminalKeyEncoder.specialKeyBytes(TerminalKeyEncoder.KC_DPAD_DOWN, applicationCursorMode = true))
    }

    @Test
    fun `DECCKM arrow right maps to SS3 C`() {
        assertArrayEquals(byteArrayOf(0x1B, 0x4F, 0x43), TerminalKeyEncoder.specialKeyBytes(TerminalKeyEncoder.KC_DPAD_RIGHT, applicationCursorMode = true))
    }

    @Test
    fun `DECCKM arrow left maps to SS3 D`() {
        assertArrayEquals(byteArrayOf(0x1B, 0x4F, 0x44), TerminalKeyEncoder.specialKeyBytes(TerminalKeyEncoder.KC_DPAD_LEFT, applicationCursorMode = true))
    }

    @Test
    fun `page up maps to CSI 5 tilde`() {
        assertArrayEquals(byteArrayOf(0x1B, 0x5B, 0x35, 0x7E), TerminalKeyEncoder.specialKeyBytes(TerminalKeyEncoder.KC_PAGE_UP))
    }

    @Test
    fun `page down maps to CSI 6 tilde`() {
        assertArrayEquals(byteArrayOf(0x1B, 0x5B, 0x36, 0x7E), TerminalKeyEncoder.specialKeyBytes(TerminalKeyEncoder.KC_PAGE_DOWN))
    }

    @Test
    fun `home maps to CSI H`() {
        assertArrayEquals(byteArrayOf(0x1B, 0x5B, 0x48), TerminalKeyEncoder.specialKeyBytes(TerminalKeyEncoder.KC_MOVE_HOME))
    }

    @Test
    fun `end maps to CSI F`() {
        assertArrayEquals(byteArrayOf(0x1B, 0x5B, 0x46), TerminalKeyEncoder.specialKeyBytes(TerminalKeyEncoder.KC_MOVE_END))
    }

    @Test
    fun `insert maps to CSI 2 tilde`() {
        assertArrayEquals(byteArrayOf(0x1B, 0x5B, 0x32, 0x7E), TerminalKeyEncoder.specialKeyBytes(TerminalKeyEncoder.KC_INSERT))
    }

    @Test
    fun `forward delete maps to CSI 3 tilde`() {
        assertArrayEquals(byteArrayOf(0x1B, 0x5B, 0x33, 0x7E), TerminalKeyEncoder.specialKeyBytes(TerminalKeyEncoder.KC_FORWARD_DEL))
    }

    @Test
    fun `unknown keycode returns null`() {
        assertNull(TerminalKeyEncoder.specialKeyBytes(9999))
    }

    // ── F1〜F12（rust-core の terminal_function_key_bytes() と一致させる） ──

    @Test
    fun `F1 maps to SS3 P`() {
        assertArrayEquals(byteArrayOf(0x1B, 0x4F, 0x50), TerminalKeyEncoder.specialKeyBytes(TerminalKeyEncoder.KC_F1))
    }

    @Test
    fun `F2 maps to SS3 Q`() {
        assertArrayEquals(byteArrayOf(0x1B, 0x4F, 0x51), TerminalKeyEncoder.specialKeyBytes(TerminalKeyEncoder.KC_F2))
    }

    @Test
    fun `F3 maps to SS3 R`() {
        assertArrayEquals(byteArrayOf(0x1B, 0x4F, 0x52), TerminalKeyEncoder.specialKeyBytes(TerminalKeyEncoder.KC_F3))
    }

    @Test
    fun `F4 maps to SS3 S`() {
        assertArrayEquals(byteArrayOf(0x1B, 0x4F, 0x53), TerminalKeyEncoder.specialKeyBytes(TerminalKeyEncoder.KC_F4))
    }

    @Test
    fun `F5 maps to CSI 15 tilde`() {
        assertArrayEquals(byteArrayOf(0x1B, 0x5B, 0x31, 0x35, 0x7E), TerminalKeyEncoder.specialKeyBytes(TerminalKeyEncoder.KC_F5))
    }

    @Test
    fun `F6 maps to CSI 17 tilde`() {
        assertArrayEquals(byteArrayOf(0x1B, 0x5B, 0x31, 0x37, 0x7E), TerminalKeyEncoder.specialKeyBytes(TerminalKeyEncoder.KC_F6))
    }

    @Test
    fun `F7 maps to CSI 18 tilde`() {
        assertArrayEquals(byteArrayOf(0x1B, 0x5B, 0x31, 0x38, 0x7E), TerminalKeyEncoder.specialKeyBytes(TerminalKeyEncoder.KC_F7))
    }

    @Test
    fun `F8 maps to CSI 19 tilde`() {
        assertArrayEquals(byteArrayOf(0x1B, 0x5B, 0x31, 0x39, 0x7E), TerminalKeyEncoder.specialKeyBytes(TerminalKeyEncoder.KC_F8))
    }

    @Test
    fun `F9 maps to CSI 20 tilde`() {
        assertArrayEquals(byteArrayOf(0x1B, 0x5B, 0x32, 0x30, 0x7E), TerminalKeyEncoder.specialKeyBytes(TerminalKeyEncoder.KC_F9))
    }

    @Test
    fun `F10 maps to CSI 21 tilde`() {
        assertArrayEquals(byteArrayOf(0x1B, 0x5B, 0x32, 0x31, 0x7E), TerminalKeyEncoder.specialKeyBytes(TerminalKeyEncoder.KC_F10))
    }

    @Test
    fun `F11 maps to CSI 23 tilde`() {
        assertArrayEquals(byteArrayOf(0x1B, 0x5B, 0x32, 0x33, 0x7E), TerminalKeyEncoder.specialKeyBytes(TerminalKeyEncoder.KC_F11))
    }

    @Test
    fun `F12 maps to CSI 24 tilde`() {
        assertArrayEquals(byteArrayOf(0x1B, 0x5B, 0x32, 0x34, 0x7E), TerminalKeyEncoder.specialKeyBytes(TerminalKeyEncoder.KC_F12))
    }

    // ── unicodeCharBytes ───────────────────────────────────────────

    @Test
    fun `zero codepoint returns null`() {
        assertNull(TerminalKeyEncoder.unicodeCharBytes(0))
    }

    @Test
    fun `control char 0x03 stays as single byte`() {
        assertArrayEquals(byteArrayOf(0x03), TerminalKeyEncoder.unicodeCharBytes(0x03))
    }

    @Test
    fun `ASCII letter encodes as UTF-8`() {
        assertArrayEquals("a".toByteArray(), TerminalKeyEncoder.unicodeCharBytes('a'.code))
    }

    @Test
    fun `Japanese char encodes as UTF-8`() {
        val expected = "あ".toByteArray(Charsets.UTF_8)
        assertArrayEquals(expected, TerminalKeyEncoder.unicodeCharBytes('あ'.code))
    }

    // ── commitTextBytes ────────────────────────────────────────────

    @Test
    fun `empty string returns empty bytes`() {
        assertEquals(0, TerminalKeyEncoder.commitTextBytes("").size)
    }

    @Test
    fun `single char encodes as plain UTF-8`() {
        assertArrayEquals("a".toByteArray(), TerminalKeyEncoder.commitTextBytes("a"))
    }

    @Test
    fun `multi char without bracketedPasteMode encodes as plain UTF-8`() {
        assertArrayEquals("ab".toByteArray(), TerminalKeyEncoder.commitTextBytes("ab"))
    }

    @Test
    fun `multi char wraps in bracketed paste when mode enabled`() {
        val bytes = TerminalKeyEncoder.commitTextBytes("ab", bracketedPasteMode = true)
        // ESC[200~ (0x1B 0x5B ...) で始まること
        assertEquals(0x1B.toByte(), bytes[0])
        val text = bytes.toString(Charsets.UTF_8)
        assertTrue(text.contains("ab"))
        // ESC[201~ で終わること
        assertEquals(0x7E.toByte(), bytes.last())
    }

    @Test
    fun `japanese multi char wraps in bracketed paste when mode enabled`() {
        val bytes = TerminalKeyEncoder.commitTextBytes("テスト", bracketedPasteMode = true)
        assertEquals(0x1B.toByte(), bytes[0])
        val text = bytes.toString(Charsets.UTF_8)
        assertTrue(text.contains("テスト"))
        assertEquals(0x7E.toByte(), bytes.last())
    }

    @Test
    fun `emoji single codepoint does not wrap even with bracketedPasteMode`() {
        val emoji = "😀"  // 😀 — 2 UTF-16 chars, 1 codepoint
        val bytes = TerminalKeyEncoder.commitTextBytes(emoji, bracketedPasteMode = true)
        assertArrayEquals(emoji.toByteArray(Charsets.UTF_8), bytes)
    }

    // ── ctrlByte（トグル式 Ctrl キー） ────────────────────────────

    @Test
    fun `lowercase a maps to 0x01`() {
        assertArrayEquals(byteArrayOf(0x01), TerminalKeyEncoder.ctrlByte('a'.code))
    }

    @Test
    fun `uppercase A maps to 0x01`() {
        assertArrayEquals(byteArrayOf(0x01), TerminalKeyEncoder.ctrlByte('A'.code))
    }

    @Test
    fun `lowercase z maps to 0x1A`() {
        assertArrayEquals(byteArrayOf(0x1A), TerminalKeyEncoder.ctrlByte('z'.code))
    }

    @Test
    fun `at sign maps to 0x00`() {
        assertArrayEquals(byteArrayOf(0x00), TerminalKeyEncoder.ctrlByte('@'.code))
    }

    @Test
    fun `open bracket maps to ESC 0x1B`() {
        assertArrayEquals(byteArrayOf(0x1B), TerminalKeyEncoder.ctrlByte('['.code))
    }

    @Test
    fun `question mark maps to DEL 0x7F`() {
        assertArrayEquals(byteArrayOf(0x7F), TerminalKeyEncoder.ctrlByte('?'.code))
    }

    @Test
    fun `space maps to NUL 0x00`() {
        assertArrayEquals(byteArrayOf(0x00), TerminalKeyEncoder.ctrlByte(' '.code))
    }

    @Test
    fun `digit returns null`() {
        assertNull(TerminalKeyEncoder.ctrlByte('1'.code))
    }

    @Test
    fun `japanese char returns null`() {
        assertNull(TerminalKeyEncoder.ctrlByte('あ'.code))
    }

    // ── commitTextBytes 改行正規化（クリップボードペースト経路）─────

    @Test
    fun `bare LF is normalized to CR`() {
        val bytes = TerminalKeyEncoder.commitTextBytes("a\nb")
        assertArrayEquals("a\rb".toByteArray(Charsets.UTF_8), bytes)
    }

    @Test
    fun `CRLF is normalized to single CR not CRCR`() {
        val bytes = TerminalKeyEncoder.commitTextBytes("a\r\nb")
        assertArrayEquals("a\rb".toByteArray(Charsets.UTF_8), bytes)
    }

    @Test
    fun `multiple lines are all normalized`() {
        val bytes = TerminalKeyEncoder.commitTextBytes("line1\r\nline2\nline3")
        assertArrayEquals("line1\rline2\rline3".toByteArray(Charsets.UTF_8), bytes)
    }

    @Test
    fun `newline normalization happens before bracketed paste wrapping`() {
        val bytes = TerminalKeyEncoder.commitTextBytes("a\r\nb", bracketedPasteMode = true)
        val text = bytes.toString(Charsets.UTF_8)
        // ペイロード部分は正規化済み（CR 1個）であり、生の CRLF を含まない
        assertTrue(text.contains("a\rb"))
        assertTrue(!text.contains("\r\n"))
        assertEquals(0x1B.toByte(), bytes[0])
        assertEquals(0x7E.toByte(), bytes.last())
    }

    @Test
    fun `single line paste without bracketedPasteMode is unaffected by normalization`() {
        assertArrayEquals("no-newline-here".toByteArray(), TerminalKeyEncoder.commitTextBytes("no-newline-here"))
    }

    // ── jisSpecialKeyBytes（JIS配列固有キー: ¥/ろ）─────────────────

    @Test
    fun `yen key unshifted maps to backslash`() {
        assertArrayEquals(byteArrayOf(0x5C), TerminalKeyEncoder.jisSpecialKeyBytes(TerminalKeyEncoder.KC_YEN, shiftPressed = false))
    }

    @Test
    fun `yen key shifted maps to pipe`() {
        assertArrayEquals(byteArrayOf(0x7C), TerminalKeyEncoder.jisSpecialKeyBytes(TerminalKeyEncoder.KC_YEN, shiftPressed = true))
    }

    @Test
    fun `ro key unshifted maps to backslash`() {
        assertArrayEquals(byteArrayOf(0x5C), TerminalKeyEncoder.jisSpecialKeyBytes(TerminalKeyEncoder.KC_RO, shiftPressed = false))
    }

    @Test
    fun `ro key shifted maps to underscore`() {
        assertArrayEquals(byteArrayOf(0x5F), TerminalKeyEncoder.jisSpecialKeyBytes(TerminalKeyEncoder.KC_RO, shiftPressed = true))
    }

    @Test
    fun `non-jis keycode returns null from jisSpecialKeyBytes`() {
        assertNull(TerminalKeyEncoder.jisSpecialKeyBytes(TerminalKeyEncoder.KC_ENTER, shiftPressed = false))
        assertNull(TerminalKeyEncoder.jisSpecialKeyBytes(9999, shiftPressed = true))
    }

    // ── altKeyBytes（物理 Alt/Meta 修飾キー: meta sends escape） ────

    @Test
    fun `alt plus lowercase b prefixes ESC`() {
        assertArrayEquals(byteArrayOf(0x1B, 'b'.code.toByte()), TerminalKeyEncoder.altKeyBytes('b'.code))
    }

    @Test
    fun `alt plus uppercase encodes as ESC plus UTF-8`() {
        assertArrayEquals(byteArrayOf(0x1B) + "F".toByteArray(Charsets.UTF_8), TerminalKeyEncoder.altKeyBytes('F'.code))
    }

    @Test
    fun `alt plus zero codepoint returns null`() {
        assertNull(TerminalKeyEncoder.altKeyBytes(0))
    }

    @Test
    fun `alt plus control char prefixes ESC before the raw control byte`() {
        assertArrayEquals(byteArrayOf(0x1B, 0x03), TerminalKeyEncoder.altKeyBytes(0x03))
    }

    // ── specialKeyBytes with modifiers（rust-core `terminal_special_key_bytes`(タスク#29)
    //    と同一golden表。rust-core/src/lib.rs のテストと1対1で対応させている） ──────

    private val ctrlMod = TerminalKeyModifiers(shift = false, alt = false, ctrl = true, meta = false)
    private val shiftMod = TerminalKeyModifiers(shift = true, alt = false, ctrl = false, meta = false)
    private val shiftCtrlMod = TerminalKeyModifiers(shift = true, alt = false, ctrl = true, meta = false)

    @Test
    fun `ctrl plus arrow keys always use CSI form regardless of DECCKM`() {
        // DECCKM無効時
        assertArrayEquals(
            byteArrayOf(0x1B, 0x5B, 0x31, 0x3B, 0x35, 0x41),
            TerminalKeyEncoder.specialKeyBytes(TerminalKeyEncoder.KC_DPAD_UP, applicationCursorMode = false, modifiers = ctrlMod),
        )
        // DECCKM有効時でも修飾子付きはSS3にならずCSI形式のまま
        assertArrayEquals(
            byteArrayOf(0x1B, 0x5B, 0x31, 0x3B, 0x35, 0x41),
            TerminalKeyEncoder.specialKeyBytes(TerminalKeyEncoder.KC_DPAD_UP, applicationCursorMode = true, modifiers = ctrlMod),
        )
        assertArrayEquals(
            byteArrayOf(0x1B, 0x5B, 0x31, 0x3B, 0x35, 0x42),
            TerminalKeyEncoder.specialKeyBytes(TerminalKeyEncoder.KC_DPAD_DOWN, applicationCursorMode = true, modifiers = ctrlMod),
        )
        assertArrayEquals(
            byteArrayOf(0x1B, 0x5B, 0x31, 0x3B, 0x35, 0x43),
            TerminalKeyEncoder.specialKeyBytes(TerminalKeyEncoder.KC_DPAD_RIGHT, applicationCursorMode = true, modifiers = ctrlMod),
        )
        assertArrayEquals(
            byteArrayOf(0x1B, 0x5B, 0x31, 0x3B, 0x35, 0x44),
            TerminalKeyEncoder.specialKeyBytes(TerminalKeyEncoder.KC_DPAD_LEFT, applicationCursorMode = true, modifiers = ctrlMod),
        )
    }

    @Test
    fun `shift plus arrow up uses xterm modifier 2`() {
        assertArrayEquals(
            byteArrayOf(0x1B, 0x5B, 0x31, 0x3B, 0x32, 0x41),
            TerminalKeyEncoder.specialKeyBytes(TerminalKeyEncoder.KC_DPAD_UP, applicationCursorMode = false, modifiers = shiftMod),
        )
    }

    @Test
    fun `home end with modifiers use parameterized CSI`() {
        assertArrayEquals(
            byteArrayOf(0x1B, 0x5B, 0x31, 0x3B, 0x35, 0x48),
            TerminalKeyEncoder.specialKeyBytes(TerminalKeyEncoder.KC_MOVE_HOME, modifiers = ctrlMod),
        )
        assertArrayEquals(
            byteArrayOf(0x1B, 0x5B, 0x31, 0x3B, 0x35, 0x46),
            TerminalKeyEncoder.specialKeyBytes(TerminalKeyEncoder.KC_MOVE_END, modifiers = ctrlMod),
        )
    }

    @Test
    fun `page up down with modifiers use parameterized tilde`() {
        assertArrayEquals(
            "\u001B[5;5~".toByteArray(Charsets.US_ASCII),
            TerminalKeyEncoder.specialKeyBytes(TerminalKeyEncoder.KC_PAGE_UP, modifiers = ctrlMod),
        )
        assertArrayEquals(
            "\u001B[6;5~".toByteArray(Charsets.US_ASCII),
            TerminalKeyEncoder.specialKeyBytes(TerminalKeyEncoder.KC_PAGE_DOWN, modifiers = ctrlMod),
        )
    }

    @Test
    fun `F1 to F4 switch from SS3 to CSI when modified`() {
        assertArrayEquals(
            "\u001B[1;5P".toByteArray(Charsets.US_ASCII),
            TerminalKeyEncoder.specialKeyBytes(TerminalKeyEncoder.KC_F1, modifiers = ctrlMod),
        )
        assertArrayEquals(
            "\u001B[1;5S".toByteArray(Charsets.US_ASCII),
            TerminalKeyEncoder.specialKeyBytes(TerminalKeyEncoder.KC_F4, modifiers = ctrlMod),
        )
    }

    @Test
    fun `F5 to F12 use parameterized tilde when modified`() {
        assertArrayEquals(
            "\u001B[15;5~".toByteArray(Charsets.US_ASCII),
            TerminalKeyEncoder.specialKeyBytes(TerminalKeyEncoder.KC_F5, modifiers = ctrlMod),
        )
        assertArrayEquals(
            "\u001B[24;5~".toByteArray(Charsets.US_ASCII),
            TerminalKeyEncoder.specialKeyBytes(TerminalKeyEncoder.KC_F12, modifiers = ctrlMod),
        )
    }

    @Test
    fun `shift tab maps to CBT`() {
        assertArrayEquals(
            byteArrayOf(0x1B, 0x5B, 0x5A),
            TerminalKeyEncoder.specialKeyBytes(TerminalKeyEncoder.KC_TAB, modifiers = shiftMod),
        )
    }

    @Test
    fun `tab with non-shift modifiers falls back to plain tab`() {
        assertArrayEquals(byteArrayOf(0x09), TerminalKeyEncoder.specialKeyBytes(TerminalKeyEncoder.KC_TAB, modifiers = ctrlMod))
        assertArrayEquals(byteArrayOf(0x09), TerminalKeyEncoder.specialKeyBytes(TerminalKeyEncoder.KC_TAB, modifiers = shiftCtrlMod))
    }

    @Test
    fun `keys unaffected by modifiers stay the same`() {
        assertArrayEquals(byteArrayOf(0x0D), TerminalKeyEncoder.specialKeyBytes(TerminalKeyEncoder.KC_ENTER, modifiers = ctrlMod))
        assertArrayEquals(byteArrayOf(0x7F), TerminalKeyEncoder.specialKeyBytes(TerminalKeyEncoder.KC_DEL, modifiers = ctrlMod))
        assertArrayEquals(byteArrayOf(0x1B), TerminalKeyEncoder.specialKeyBytes(TerminalKeyEncoder.KC_ESCAPE, modifiers = ctrlMod))
    }
}
