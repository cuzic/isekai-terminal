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

/** [DeletableListViewModel]をそのままテストするための最小限のサブクラス。 */
private class FakeDeletableListViewModel(
    app: Application,
    initial: List<String>,
) : DeletableListViewModel<String>(app) {
    val deleted = mutableListOf<String>()
    var loadCallCount = 0
    private val backing = initial.toMutableList()

    override suspend fun fetchAll(): List<String> {
        loadCallCount++
        return backing.toList()
    }

    override suspend fun deleteItem(item: String) {
        deleted.add(item)
        backing.remove(item)
    }
}

/**
 * ProfileListViewModel/KeyListViewModel/SnippetListViewModelが共有する
 * [DeletableListViewModel]の削除確認フロー(requestDelete/dismissDelete/confirmDelete)を
 * 基底クラス1本でテストする。個々のサブクラスのテストが暗黙に検証していた共通契約を
 * ここに集約し、重複を減らす。
 */
@OptIn(ExperimentalCoroutinesApi::class)
@RunWith(RobolectricTestRunner::class)
@Config(sdk = [28])
class DeletableListViewModelTest {
    private lateinit var vm: FakeDeletableListViewModel

    @Before
    fun setup() {
        Dispatchers.setMain(UnconfinedTestDispatcher())
        val app = ApplicationProvider.getApplicationContext<Application>()
        vm = FakeDeletableListViewModel(app, listOf("a", "b", "c"))
    }

    @After fun teardown() { Dispatchers.resetMain() }

    // load()/confirmDelete()はviewModelScope.launch(Dispatchers.IO)で実行される
    // (テストのUnconfinedTestDispatcherではなく実スレッドプール)ため、ポーリングで待つ。
    private suspend fun awaitItems(predicate: (List<String>) -> Boolean) =
        withTimeout(3000) { while (!predicate(vm.items.value)) delay(10) }

    @Test
    fun load_populatesItemsFromFetchAll() = runBlocking {
        assertEquals(emptyList<String>(), vm.items.value)

        vm.load()
        awaitItems { it.size == 3 }

        assertEquals(listOf("a", "b", "c"), vm.items.value)
    }

    @Test
    fun requestDelete_setsDeleteTarget() {
        assertNull(vm.deleteTarget.value)

        vm.requestDelete("b")

        assertEquals("b", vm.deleteTarget.value)
    }

    @Test
    fun dismissDelete_clearsDeleteTargetWithoutDeleting() {
        vm.requestDelete("b")

        vm.dismissDelete()

        assertNull(vm.deleteTarget.value)
        assertTrue(vm.deleted.isEmpty())
    }

    @Test
    fun confirmDelete_clearsDeleteTargetImmediately() {
        vm.requestDelete("b")

        vm.confirmDelete("b")

        assertNull(vm.deleteTarget.value)
    }

    @Test
    fun confirmDelete_deletesItemAndReloadsFromFetchAll() = runBlocking {
        vm.load()
        awaitItems { it.size == 3 }
        val loadCallsBeforeDelete = vm.loadCallCount

        vm.confirmDelete("b")
        awaitItems { it.size == 2 }

        assertEquals(listOf("b"), vm.deleted)
        assertEquals(listOf("a", "c"), vm.items.value)
        assertTrue("confirmDelete後にload()経由でfetchAllが再度呼ばれるべき", vm.loadCallCount > loadCallsBeforeDelete)
    }
}
