package tools.isekai.terminal.pack

import org.junit.Assert.*
import org.junit.Test
import tools.isekai.terminal.KeySequenceCommands
import tools.isekai.terminal.input.KeyStep

class KeySequencePackResolverTest {

    @Test
    fun `resolves tmux pack sequences using installation param values`() {
        val paramValues = mapOf("prefix" to KeyStep.CtrlChar('b'))
        val resolved = KeySequencePackResolver.resolve(KeySequencePacks.TMUX, paramValues)

        assertEquals(KeySequencePacks.TMUX.sequences.size, resolved.size)
        val newWindow = resolved.first { it.label == "新規ウィンドウ" }
        assertEquals(listOf(KeyStep.CtrlChar('b'), KeyStep.Text("c")), newWindow.steps)
        assertEquals("tmux", newWindow.packId)
    }

    @Test
    fun `changing prefix param immediately changes all resolved sequences (live binding)`() {
        // ユーザーがprefixキーを Ctrl+B から Ctrl+A(screen互換)へ変更した場合、
        // installationのparamValuesを1箇所変えるだけでパック内の全ボタンへ反映されること
        // (有効化時に打鍵列を複製する「マテリアライズ方式」ではないことの確認)。
        val before = KeySequencePackResolver.resolve(KeySequencePacks.TMUX, mapOf("prefix" to KeyStep.CtrlChar('b')))
        val after = KeySequencePackResolver.resolve(KeySequencePacks.TMUX, mapOf("prefix" to KeyStep.CtrlChar('a')))

        for (seq in before) {
            assertTrue(seq.steps.contains(KeyStep.CtrlChar('b')))
        }
        for (seq in after) {
            assertTrue(seq.steps.contains(KeyStep.CtrlChar('a')))
            assertFalse(seq.steps.contains(KeyStep.CtrlChar('b')))
        }
    }

    @Test
    fun `missing param value falls back to the pack's default`() {
        val resolved = KeySequencePackResolver.resolve(KeySequencePacks.TMUX, emptyMap())
        val newWindow = resolved.first { it.label == "新規ウィンドウ" }
        // TMUXパックのdefaultはCtrl+B。
        assertEquals(listOf(KeyStep.CtrlChar('b'), KeyStep.Text("c")), newWindow.steps)
    }

    @Test
    fun `unknown placeholder name with no default is left unresolved and produces no bytes`() {
        val pack = KeySequencePack(
            id = "test", version = 1, name = "test",
            params = emptyList(), // "prefix" という名前のparamは定義されていない
            sequences = listOf(PackSequenceTemplate("x", listOf(KeyStep.PlaceholderRef("prefix"), KeyStep.Text("c")))),
        )
        val resolved = KeySequencePackResolver.resolve(pack, emptyMap())
        assertEquals(listOf(KeyStep.PlaceholderRef("prefix"), KeyStep.Text("c")), resolved.single().steps)
        // 未解決のPlaceholderRefはKeySequenceCommands.toBytesで何も出力しない(安全側の挙動)。
        assertArrayEquals(byteArrayOf('c'.code.toByte()), KeySequenceCommands.toBytes(resolved.single().steps))
    }

    @Test
    fun `tmux pack sequence with a double-quote step resolves correctly`() {
        val resolved = KeySequencePackResolver.resolve(KeySequencePacks.TMUX, mapOf("prefix" to KeyStep.CtrlChar('b')))
        val splitHorizontal = resolved.first { it.label == "ペイン分割(上下)" }
        assertEquals(listOf(KeyStep.CtrlChar('b'), KeyStep.Text("\"")), splitHorizontal.steps)
        assertArrayEquals(byteArrayOf(0x02, '"'.code.toByte()), KeySequenceCommands.toBytes(splitHorizontal.steps))
    }
}
