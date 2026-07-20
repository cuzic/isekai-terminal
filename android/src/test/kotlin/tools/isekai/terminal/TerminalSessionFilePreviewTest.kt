package tools.isekai.terminal

import kotlinx.coroutines.async
import kotlinx.coroutines.delay
import kotlinx.coroutines.runBlocking
import kotlinx.coroutines.withTimeout
import org.junit.After
import org.junit.Assert.assertEquals
import org.junit.Assert.assertTrue
import org.junit.Before
import org.junit.Test
import tools.isekai.terminal.session.TerminalSession
import uniffi.isekai_terminal_core.FilePreviewEntry
import uniffi.isekai_terminal_core.FilePreviewOutcome
import uniffi.isekai_terminal_core.FilePreviewRequestKind

/**
 * タスク#17(ファイルプレビュー機能): `TerminalSession.filePreviewRequest`が
 * `SessionOrchestrator.filePreviewRequest`(request_id発行)→`onFilePreviewResult`
 * コールバック(`FakeOrchestrator.simulateFilePreviewResult`で模擬)を介して正しく
 * `CompletableDeferred`を解決することを確認する。パース/デコード自体はRust側の
 * 責務(`rust-core/src/file_preview.rs`)なのでここでは検証しない。
 */
class TerminalSessionFilePreviewTest {

    private lateinit var fakeOrchestrator: FakeOrchestrator
    private lateinit var session: TerminalSession

    @Before
    fun setup() {
        fakeOrchestrator = FakeOrchestrator()
        session = TerminalSession(FakeHostKeyChecker(), orchestratorFactory = { cb -> fakeOrchestrator.also { it.callback = cb } })
    }

    @After
    fun teardown() {
        session.close()
    }

    @Test
    fun `filePreviewRequest resolves with the outcome delivered via the callback`() = runBlocking {
        val deferredResult = async {
            withTimeout(3000) { session.filePreviewRequest(FilePreviewRequestKind.Ls("/tmp")) }
        }
        // orchestrator.filePreviewRequestが呼ばれ、request_idが発行されるまで待つ。
        withTimeout(3000) {
            while (fakeOrchestrator.filePreviewRequests.isEmpty()) delay(10)
        }
        val (requestId, kind) = fakeOrchestrator.filePreviewRequests.single()
        assertEquals(FilePreviewRequestKind.Ls("/tmp"), kind)

        val entries = listOf(FilePreviewEntry("a.txt", false, false, 3uL, null))
        fakeOrchestrator.simulateFilePreviewResult(requestId, FilePreviewOutcome.Ls(entries))

        val outcome = deferredResult.await()
        assertEquals(FilePreviewOutcome.Ls(entries), outcome)
    }

    @Test
    fun `concurrent requests resolve independently by request_id`() = runBlocking {
        val first = async { withTimeout(3000) { session.filePreviewRequest(FilePreviewRequestKind.Ls("/a")) } }
        val second = async { withTimeout(3000) { session.filePreviewRequest(FilePreviewRequestKind.Ls("/b")) } }

        withTimeout(3000) {
            while (fakeOrchestrator.filePreviewRequests.size < 2) delay(10)
        }
        val requests = fakeOrchestrator.filePreviewRequests.toList()
        val idForA = requests.first { it.second == FilePreviewRequestKind.Ls("/a") }.first
        val idForB = requests.first { it.second == FilePreviewRequestKind.Ls("/b") }.first

        // 到着順を逆にしても取り違えないことを確認する。
        fakeOrchestrator.simulateFilePreviewResult(idForB, FilePreviewOutcome.Error("b failed"))
        fakeOrchestrator.simulateFilePreviewResult(idForA, FilePreviewOutcome.Ls(emptyList()))

        assertEquals(FilePreviewOutcome.Ls(emptyList()), first.await())
        assertTrue((second.await() as FilePreviewOutcome.Error).message == "b failed")
    }
}
