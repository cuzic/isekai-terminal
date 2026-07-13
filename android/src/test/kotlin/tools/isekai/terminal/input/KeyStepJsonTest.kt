package tools.isekai.terminal.input

import org.junit.Assert.*
import org.junit.Test
import org.junit.runner.RunWith
import org.robolectric.RobolectricTestRunner
import org.robolectric.annotation.Config

/**
 * org.json(JSONArray/JSONObject)は素の JVM unit test では android.jar のスタブ実装
 * (呼ぶと例外)になるため、[PortForwardListConverterTest] と同じく Robolectric 経由で走らせる。
 */
@RunWith(RobolectricTestRunner::class)
@Config(sdk = [28])
class KeyStepJsonTest {

    // ── 往復(encode → decode) ─────────────────────────────

    @Test
    fun `round-trips CtrlChar`() {
        val steps = listOf(KeyStep.CtrlChar('b'))
        assertEquals(steps, KeyStepJson.decode(KeyStepJson.encode(steps)))
    }

    @Test
    fun `round-trips Text including a double-quote character`() {
        // tmux パックの「ペイン分割(上下)」ステップ相当。JSON エスケープでハマりやすい箇所。
        val steps = listOf(KeyStep.PlaceholderRef("prefix"), KeyStep.Text("\""))
        assertEquals(steps, KeyStepJson.decode(KeyStepJson.encode(steps)))
    }

    @Test
    fun `round-trips Special`() {
        val steps = listOf(KeyStep.Special(TerminalKeyEncoder.KC_F5))
        assertEquals(steps, KeyStepJson.decode(KeyStepJson.encode(steps)))
    }

    @Test
    fun `round-trips PlaceholderRef`() {
        val steps = listOf(KeyStep.PlaceholderRef("prefix"))
        assertEquals(steps, KeyStepJson.decode(KeyStepJson.encode(steps)))
    }

    @Test
    fun `round-trips full tmux new-window sequence`() {
        val steps = listOf(KeyStep.PlaceholderRef("prefix"), KeyStep.Text("c"))
        val json = KeyStepJson.encode(steps)
        assertEquals(steps, KeyStepJson.decode(json))
    }

    @Test
    fun `round-trip preserves byte output for a sequence containing a quote`() {
        val steps = listOf(KeyStep.CtrlChar('b'), KeyStep.Text("\""))
        val before = KeySequenceCommandsBridgeToBytes(steps)
        val restored = KeyStepJson.decode(KeyStepJson.encode(steps))
        val after = KeySequenceCommandsBridgeToBytes(restored)
        assertArrayEquals(before, after)
    }

    // ── 空/壊れたJSON ─────────────────────────────────────

    @Test
    fun `empty string decodes to empty list`() {
        assertEquals(emptyList<KeyStep>(), KeyStepJson.decode(""))
    }

    @Test
    fun `blank string decodes to empty list`() {
        assertEquals(emptyList<KeyStep>(), KeyStepJson.decode("   "))
    }

    @Test
    fun `malformed json decodes to empty list instead of throwing`() {
        assertEquals(emptyList<KeyStep>(), KeyStepJson.decode("{not valid json"))
    }

    @Test
    fun `non-array json decodes to empty list instead of throwing`() {
        assertEquals(emptyList<KeyStep>(), KeyStepJson.decode("""{"type":"text"}"""))
    }

    // ── 未知 type / 復元不能な値は該当 step のみスキップ ────────

    @Test
    fun `unknown type is skipped but sibling steps survive`() {
        val json = """[{"type":"ctrlChar","char":"b"},{"type":"future-unknown-type"},{"type":"text","text":"c"}]"""
        assertEquals(listOf(KeyStep.CtrlChar('b'), KeyStep.Text("c")), KeyStepJson.decode(json))
    }

    @Test
    fun `unknown special keyCode is skipped`() {
        val json = """[{"type":"special","keyCode":999999}]"""
        assertEquals(emptyList<KeyStep>(), KeyStepJson.decode(json))
    }

    @Test
    fun `empty placeholderRef name is skipped`() {
        val json = """[{"type":"placeholderRef","name":""}]"""
        assertEquals(emptyList<KeyStep>(), KeyStepJson.decode(json))
    }

    @Test
    fun `ctrlChar with multi-character string is skipped`() {
        val json = """[{"type":"ctrlChar","char":"bb"}]"""
        assertEquals(emptyList<KeyStep>(), KeyStepJson.decode(json))
    }
}

/** テスト専用: JSON往復前後でバイト列が変わらないことを確認するための小さなブリッジ関数。 */
private fun KeySequenceCommandsBridgeToBytes(steps: List<KeyStep>): ByteArray {
    val resolved = steps.map { step ->
        if (step is KeyStep.PlaceholderRef) KeyStep.CtrlChar('b') else step
    }
    return tools.isekai.terminal.KeySequenceCommands.toBytes(resolved)
}
