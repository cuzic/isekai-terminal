package tools.isekai.terminal

import android.app.Application
import androidx.test.core.app.ApplicationProvider
import tools.isekai.terminal.data.KeyEntry
import tools.isekai.terminal.data.Repositories
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.ExperimentalCoroutinesApi
import kotlinx.coroutines.flow.first
import kotlinx.coroutines.runBlocking
import kotlinx.coroutines.test.UnconfinedTestDispatcher
import kotlinx.coroutines.test.resetMain
import kotlinx.coroutines.test.setMain
import kotlinx.coroutines.withTimeout
import org.junit.After
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Before
import org.junit.Test
import org.junit.runner.RunWith
import org.robolectric.RobolectricTestRunner
import org.robolectric.annotation.Config

@OptIn(ExperimentalCoroutinesApi::class)
@RunWith(RobolectricTestRunner::class)
@Config(sdk = [28])
class KeyListViewModelTest {
    private lateinit var vm: KeyListViewModel

    @Before fun setup() {
        Dispatchers.setMain(UnconfinedTestDispatcher())
        val app = ApplicationProvider.getApplicationContext<Application>()
        Repositories.init(app)
        runBlocking { Repositories.keys.getAll().forEach { Repositories.keys.delete(it) } }
        vm = KeyListViewModel(app)
    }

    @After fun teardown() { Dispatchers.resetMain() }

    private fun key(label: String) = KeyEntry(
        label = label,
        publicKey = "ssh-ed25519 AAAAC3$label",
        encryptedPrivateKeyPath = "/keys/$label.enc",
        kekAlias = "kek_$label",
        createdAt = 1_700_000_000_000L,
    )

    private suspend fun awaitKeys(condition: (List<KeyEntry>) -> Boolean) =
        withTimeout(3000) { vm.keys.first { condition(it) } }

    @Test fun initialState_keysEmpty() = runBlocking {
        assertTrue(vm.keys.value.isEmpty())
    }

    @Test fun initialState_pendingDeleteNull() {
        assertNull(vm.pendingDelete.value)
    }

    @Test fun initialState_generatedPubKeyNull() {
        assertNull(vm.generatedPubKey.value)
    }

    @Test fun initialState_isGeneratingFalse() {
        assertFalse(vm.isGenerating.value)
    }

    @Test fun loadKeys_withData_updatesState() = runBlocking {
        Repositories.keys.save(key("MyKey"))
        vm.loadKeys()
        val list = awaitKeys { it.isNotEmpty() }
        assertEquals(1, list.size)
        assertEquals("MyKey", list[0].label)
    }

    @Test fun loadKeys_multiple_returnsAll() = runBlocking {
        Repositories.keys.save(key("A"))
        Repositories.keys.save(key("B"))
        vm.loadKeys()
        val list = awaitKeys { it.size == 2 }
        assertEquals(2, list.size)
    }

    @Test fun requestDelete_setsPendingDelete() {
        val k = key("ToDelete")
        vm.requestDelete(k)
        assertEquals("ToDelete", vm.pendingDelete.value?.label)
    }

    @Test fun dismissDelete_clearsPendingDelete() {
        vm.requestDelete(key("ToDelete"))
        vm.dismissDelete()
        assertNull(vm.pendingDelete.value)
    }

    @Test fun confirmDelete_removesKeyFromDb() = runBlocking {
        val id = Repositories.keys.save(key("RemoveMe"))
        vm.loadKeys()
        awaitKeys { it.isNotEmpty() }
        val toDelete = vm.keys.value.first { it.id == id }
        vm.requestDelete(toDelete)
        vm.confirmDelete(toDelete)
        awaitKeys { it.isEmpty() }
        assertNull(Repositories.keys.findById(id))
    }

    @Test fun confirmDelete_clearsPendingDelete() = runBlocking {
        val id = Repositories.keys.save(key("RemoveMe"))
        vm.loadKeys()
        awaitKeys { it.isNotEmpty() }
        val toDelete = vm.keys.value.first { it.id == id }
        vm.requestDelete(toDelete)
        vm.confirmDelete(toDelete)
        awaitKeys { it.isEmpty() }
        assertNull(vm.pendingDelete.value)
    }

    @Test fun dismissGeneratedPubKey_clearsState() {
        vm.dismissGeneratedPubKey()
        assertNull(vm.generatedPubKey.value)
    }
}
