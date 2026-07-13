package tools.isekai.terminal.pack

import org.junit.Assert.*
import org.junit.Test
import org.junit.runner.RunWith
import org.robolectric.RobolectricTestRunner
import org.robolectric.annotation.Config
import tools.isekai.terminal.input.KeyStep

/** org.json利用のため[KeyStepJsonTest]と同じくRobolectric経由で走らせる。 */
@RunWith(RobolectricTestRunner::class)
@Config(sdk = [28])
class PackParamValuesJsonTest {

    @Test
    fun `round-trips a single ctrlChar param`() {
        val values = mapOf("prefix" to KeyStep.CtrlChar('b'))
        assertEquals(values, PackParamValuesJson.decode(PackParamValuesJson.encode(values)))
    }

    @Test
    fun `round-trips multiple params`() {
        val values = mapOf(
            "prefix" to KeyStep.CtrlChar('a'),
            "secondary" to KeyStep.Special(tools.isekai.terminal.input.TerminalKeyEncoder.KC_F5),
        )
        assertEquals(values, PackParamValuesJson.decode(PackParamValuesJson.encode(values)))
    }

    @Test
    fun `empty map encodes and decodes to empty map`() {
        assertEquals(emptyMap<String, KeyStep>(), PackParamValuesJson.decode(PackParamValuesJson.encode(emptyMap())))
    }

    @Test
    fun `blank string decodes to empty map`() {
        assertEquals(emptyMap<String, KeyStep>(), PackParamValuesJson.decode(""))
    }

    @Test
    fun `malformed json decodes to empty map instead of throwing`() {
        assertEquals(emptyMap<String, KeyStep>(), PackParamValuesJson.decode("{not valid"))
    }

    @Test
    fun `unresolvable value for one key is skipped, others survive`() {
        val json = """{"prefix":{"type":"ctrlChar","char":"b"},"broken":{"type":"special","keyCode":999999}}"""
        assertEquals(mapOf("prefix" to KeyStep.CtrlChar('b')), PackParamValuesJson.decode(json))
    }
}
