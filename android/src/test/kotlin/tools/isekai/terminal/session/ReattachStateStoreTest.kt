package tools.isekai.terminal.session

import java.io.File
import kotlin.io.path.createTempDirectory
import kotlinx.coroutines.runBlocking
import org.junit.After
import org.junit.Assert.*
import org.junit.Before
import org.junit.Test
import org.junit.runner.RunWith
import org.robolectric.RobolectricTestRunner
import org.robolectric.annotation.Config

/**
 * [ReattachStateStore]のJSON永続化(タスク#14)のテスト。org.json(JSONArray/JSONObject)は
 * 素のJVM unit testではandroid.jarのスタブ実装になるため、[tools.isekai.terminal.input.KeyStepJsonTest]と
 * 同じくRobolectric経由で走らせる。
 */
@RunWith(RobolectricTestRunner::class)
@Config(sdk = [28])
class ReattachStateStoreTest {
    private lateinit var dir: File
    private lateinit var file: File
    private lateinit var store: ReattachStateStore

    @Before
    fun setup() {
        dir = createTempDirectory(prefix = "reattach-state-test").toFile()
        file = File(dir, "reattach_state.json")
        store = ReattachStateStore(file)
    }

    @After
    fun teardown() {
        dir.deleteRecursively()
    }

    private fun record(tabId: String = "tab-1", profileId: Long = 1L, savedAt: Long = 1_000L) = ReattachRecord(
        tabId = tabId,
        profileId = profileId,
        label = "example",
        reattachToken = "token-$tabId",
        savedAtUnixSecs = savedAt,
    )

    @Test
    fun `load returns empty list when file does not exist`() = runBlocking {
        assertTrue(store.load().isEmpty())
    }

    @Test
    fun `upsert then load round-trips a record`() = runBlocking {
        val r = record()
        store.upsert(r)
        assertEquals(listOf(r), store.load())
    }

    @Test
    fun `upsert replaces an existing record with the same tabId`() = runBlocking {
        store.upsert(record(savedAt = 1_000L))
        store.upsert(record(savedAt = 2_000L))
        val loaded = store.load()
        assertEquals(1, loaded.size)
        assertEquals(2_000L, loaded.single().savedAtUnixSecs)
    }

    @Test
    fun `upsert keeps records with different tabIds independent`() = runBlocking {
        store.upsert(record(tabId = "tab-1"))
        store.upsert(record(tabId = "tab-2"))
        assertEquals(setOf("tab-1", "tab-2"), store.load().map { it.tabId }.toSet())
    }

    @Test
    fun `remove deletes only the matching tabId`() = runBlocking {
        store.upsert(record(tabId = "tab-1"))
        store.upsert(record(tabId = "tab-2"))
        store.remove("tab-1")
        assertEquals(listOf("tab-2"), store.load().map { it.tabId })
    }

    @Test
    fun `remove of unknown tabId is a no-op`() = runBlocking {
        store.upsert(record(tabId = "tab-1"))
        store.remove("does-not-exist")
        assertEquals(1, store.load().size)
    }

    @Test
    fun `clear empties the store`() = runBlocking {
        store.upsert(record(tabId = "tab-1"))
        store.upsert(record(tabId = "tab-2"))
        store.clear()
        assertTrue(store.load().isEmpty())
    }

    @Test
    fun `corrupt JSON falls back to an empty list instead of throwing`() = runBlocking {
        dir.mkdirs()
        file.writeText("{not valid json")
        assertTrue(store.load().isEmpty())
    }

    @Test
    fun `blank file falls back to an empty list`() = runBlocking {
        dir.mkdirs()
        file.writeText("")
        assertTrue(store.load().isEmpty())
    }

    @Test
    fun `a record missing required fields is skipped but siblings survive`() = runBlocking {
        dir.mkdirs()
        file.writeText(
            """
            [
              {"tabId":"tab-1","profileId":1,"label":"a","reattachToken":"t1","savedAtUnixSecs":1000},
              {"tabId":"","profileId":2,"label":"b","reattachToken":"t2","savedAtUnixSecs":2000}
            ]
            """.trimIndent(),
        )
        val loaded = store.load()
        assertEquals(listOf("tab-1"), loaded.map { it.tabId })
    }

    @Test
    fun `save does not leave a temp file behind`() = runBlocking {
        store.upsert(record())
        assertTrue(file.exists())
        assertFalse(File(dir, "${file.name}.tmp").exists())
    }

    @Test
    fun `writes survive re-reading through a fresh store instance`() = runBlocking {
        store.upsert(record(tabId = "tab-1"))
        val reopened = ReattachStateStore(file)
        assertEquals(listOf("tab-1"), reopened.load().map { it.tabId })
    }
}
