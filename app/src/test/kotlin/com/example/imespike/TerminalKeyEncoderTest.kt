package com.example.imespike

import com.example.imespike.input.TerminalKeyEncoder
import org.junit.Assert.*
import org.junit.Test

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
    fun `unknown keycode returns null`() {
        assertNull(TerminalKeyEncoder.specialKeyBytes(9999))
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
}
