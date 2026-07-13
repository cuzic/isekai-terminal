package tools.isekai.terminal.session

import org.junit.Assert.assertEquals
import org.junit.Assert.assertNull
import org.junit.Test
import tools.isekai.terminal.TrzszUiState
import uniffi.isekai_terminal_core.TrzszPublicState

class TrzszStateMapperTest {

    @Test
    fun `Idle maps to null`() {
        assertNull(TrzszStateMapper.toUiState(TrzszPublicState.Idle))
    }

    @Test
    fun `WaitingUser maps fields verbatim`() {
        val result = TrzszStateMapper.toUiState(
            TrzszPublicState.WaitingUser("t1", "upload", "file.txt", 1234uL),
        )
        assertEquals(TrzszUiState.WaitingUser("t1", "upload", "file.txt", 1234uL), result)
    }

    @Test
    fun `InProgress maps fields verbatim`() {
        val result = TrzszStateMapper.toUiState(
            TrzszPublicState.InProgress("t1", "download", "file.txt", 500uL, 1000uL),
        )
        assertEquals(TrzszUiState.InProgress("t1", "download", "file.txt", 500uL, 1000uL), result)
    }

    @Test
    fun `Done maps fields verbatim`() {
        val result = TrzszStateMapper.toUiState(TrzszPublicState.Done("t1", true, "完了"))
        assertEquals(TrzszUiState.Done("t1", true, "完了"), result)
    }
}
