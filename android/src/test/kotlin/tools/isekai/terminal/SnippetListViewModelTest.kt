package tools.isekai.terminal

import android.app.Application
import androidx.test.core.app.ApplicationProvider
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.ExperimentalCoroutinesApi
import kotlinx.coroutines.delay
import kotlinx.coroutines.runBlocking
import kotlinx.coroutines.test.UnconfinedTestDispatcher
import kotlinx.coroutines.test.resetMain
import kotlinx.coroutines.test.setMain
import kotlinx.coroutines.withTimeout
import org.junit.After
import org.junit.Assert.assertEquals
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Before
import org.junit.Test
import org.junit.runner.RunWith
import org.robolectric.RobolectricTestRunner
import org.robolectric.annotation.Config
import tools.isekai.terminal.data.Repositories
import tools.isekai.terminal.data.Snippet

/**
 * [SnippetListViewModel]自体の専用テストは(リファクタ前から)存在しなかった。削除UIの挙動は
 * [SnippetListScreenTest]で既に検証済みのため、ここでは[DeletableListViewModel]経由の
 * 基本的なロード/削除がRoom(Repositories.snippets)と正しく繋がっていることだけを確認する。
 */
@OptIn(ExperimentalCoroutinesApi::class)
@RunWith(RobolectricTestRunner::class)
@Config(sdk = [28])
class SnippetListViewModelTest {
    private lateinit var vm: SnippetListViewModel

    @Before
    fun setup() {
        Dispatchers.setMain(UnconfinedTestDispatcher())
        val app = ApplicationProvider.getApplicationContext<Application>()
        Repositories.init(app)
        runBlocking { Repositories.snippets.getAll().forEach { Repositories.snippets.delete(it) } }
    }

    @After fun teardown() { Dispatchers.resetMain() }

    private fun snippet(label: String) = Snippet(label = label, command = "echo $label")

    private suspend fun awaitSnippets(predicate: (List<Snippet>) -> Boolean) =
        withTimeout(3000) { while (!predicate(vm.snippets.value)) delay(10) }

    @Test
    fun init_loadsExistingSnippetsFromRoom() = runBlocking {
        runBlocking { Repositories.snippets.save(snippet("ls")) }

        vm = SnippetListViewModel(ApplicationProvider.getApplicationContext())
        awaitSnippets { it.size == 1 }

        assertEquals("ls", vm.snippets.value.first().label)
    }

    @Test
    fun confirmDelete_removesSnippetFromRoomAndReloads() = runBlocking {
        val saved = runBlocking {
            val id = Repositories.snippets.save(snippet("ll"))
            snippet("ll").copy(id = id)
        }
        vm = SnippetListViewModel(ApplicationProvider.getApplicationContext())
        awaitSnippets { it.size == 1 }

        vm.confirmDelete(saved)
        awaitSnippets { it.isEmpty() }

        assertTrue(Repositories.snippets.getAll().isEmpty())
        assertNull(vm.deleteTarget.value)
    }
}
