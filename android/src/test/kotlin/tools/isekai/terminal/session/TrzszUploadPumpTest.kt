package tools.isekai.terminal.session

import java.io.ByteArrayInputStream
import java.util.Collections
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.async
import kotlinx.coroutines.delay
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.runBlocking
import kotlinx.coroutines.withTimeout
import org.junit.Assert.assertArrayEquals
import org.junit.Assert.assertEquals
import org.junit.Assert.assertTrue
import org.junit.Test
import tools.isekai.terminal.TrzszUiState

/** AND-M1a: [TrzszUploadPump]のack駆動フロー制御。 */
class TrzszUploadPumpTest {

    private data class Sent(val size: Int, val isLast: Boolean)

    private fun inProgress(transferred: Long) =
        TrzszUiState.InProgress("t1", "upload", null, transferred.toULong(), null)

    private val chunk = 10
    private val window = 3L * chunk

    @Test
    fun sendsAtMostWindowAheadOfAckAndResumesAsAcksArrive() = runBlocking {
        val state = MutableStateFlow<TrzszUiState?>(TrzszUiState.WaitingUser("t1", "upload", null, null))
        val data = ByteArray(10 * chunk) { it.toByte() }
        val sent = Collections.synchronizedList(mutableListOf<Sent>())

        val job = async(Dispatchers.Default) {
            TrzszUploadPump.pump(ByteArrayInputStream(data), state, { d, last -> sent += Sent(d.size, last) }, chunk, window, 5_000)
        }

        withTimeout(3000) { while (sent.size < 3) delay(5) }
        delay(100)
        assertEquals("ack前はウィンドウ分だけ先行送信して止まる", 3, sent.size)

        state.value = inProgress(2L * chunk)
        withTimeout(3000) { while (sent.size < 5) delay(5) }
        delay(100)
        assertEquals(5, sent.size)

        state.value = inProgress(10L * chunk)
        assertEquals(TrzszUploadPump.Result.COMPLETED, withTimeout(3000) { job.await() })
        assertEquals(10, sent.size)
        assertTrue("最終チャンクだけisLast", sent.last().isLast)
        assertEquals(1, sent.count { it.isLast })
        assertEquals(data.size, sent.sumOf { it.size })
    }

    @Test
    fun stopsReadingWhenTransferIsCancelled() = runBlocking {
        val state = MutableStateFlow<TrzszUiState?>(inProgress(0))
        val sent = Collections.synchronizedList(mutableListOf<Sent>())

        val job = async(Dispatchers.Default) {
            TrzszUploadPump.pump(ByteArrayInputStream(ByteArray(100 * chunk)), state, { d, last -> sent += Sent(d.size, last) }, chunk, window, 5_000)
        }
        withTimeout(3000) { while (sent.size < 3) delay(5) }

        state.value = null // ユーザーのキャンセル(TerminalSession.trzszCancelがnullにする)

        assertEquals(TrzszUploadPump.Result.STOPPED, withTimeout(3000) { job.await() })
        assertEquals(3, sent.size)
        assertTrue(sent.none { it.isLast })
    }

    @Test
    fun stopsWhenTransferFinishesEarly() = runBlocking {
        val state = MutableStateFlow<TrzszUiState?>(inProgress(0))
        val job = async(Dispatchers.Default) {
            TrzszUploadPump.pump(ByteArrayInputStream(ByteArray(100 * chunk)), state, { _, _ -> }, chunk, window, 5_000)
        }
        delay(50)
        state.value = TrzszUiState.Done("t1", false, "remote aborted")
        assertEquals(TrzszUploadPump.Result.STOPPED, withTimeout(3000) { job.await() })
    }

    @Test
    fun timesOutWhenAcksNeverProgress() = runBlocking {
        val state = MutableStateFlow<TrzszUiState?>(inProgress(0))
        val result = withTimeout(3000) {
            TrzszUploadPump.pump(ByteArrayInputStream(ByteArray(100 * chunk)), state, { _, _ -> }, chunk, window, 200)
        }
        assertEquals(TrzszUploadPump.Result.TIMED_OUT, result)
    }

    @Test
    fun emptyFile_sendsSingleEmptyLastChunk() = runBlocking {
        val state = MutableStateFlow<TrzszUiState?>(inProgress(0))
        val sent = mutableListOf<Pair<ByteArray, Boolean>>()
        val result = TrzszUploadPump.pump(ByteArrayInputStream(ByteArray(0)), state, { d, last -> sent += d to last }, chunk, window, 1_000)
        assertEquals(TrzszUploadPump.Result.COMPLETED, result)
        assertEquals(1, sent.size)
        assertArrayEquals(ByteArray(0), sent[0].first)
        assertTrue(sent[0].second)
    }
}
