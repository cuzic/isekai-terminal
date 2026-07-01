package tools.isekai.terminal

import android.app.Application
import androidx.test.core.app.ApplicationProvider
import tools.isekai.terminal.data.ConnectionProfile
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
import org.junit.Assert.assertTrue
import org.junit.Before
import org.junit.Test
import org.junit.runner.RunWith
import org.robolectric.RobolectricTestRunner
import org.robolectric.annotation.Config

@OptIn(ExperimentalCoroutinesApi::class)
@RunWith(RobolectricTestRunner::class)
@Config(sdk = [28])
class ProfileEditViewModelTest {
    private lateinit var vm: ProfileEditViewModel

    @Before fun setup() {
        Dispatchers.setMain(UnconfinedTestDispatcher())
        val app = ApplicationProvider.getApplicationContext<Application>()
        Repositories.init(app)
        runBlocking {
            Repositories.profiles.getAll().forEach { Repositories.profiles.delete(it) }
            Repositories.keys.getAll().forEach { Repositories.keys.delete(it) }
        }
        vm = ProfileEditViewModel(app)
    }

    @After fun teardown() { Dispatchers.resetMain() }

    private fun sampleProfile() = ConnectionProfile(
        label = "Prod", host = "prod.example.com", username = "deploy", authType = "password",
    )

    private fun sampleKey(label: String) = KeyEntry(
        label = label,
        publicKey = "ssh-ed25519 AAAAC3$label",
        encryptedPrivateKeyPath = "/keys/$label.enc",
        kekAlias = "kek_$label",
        createdAt = 1_700_000_000_000L,
    )

    private suspend fun awaitKeys(condition: (List<KeyEntry>) -> Boolean) =
        withTimeout(3000) { vm.keys.first { condition(it) } }

    @Test fun initialState_isSavingFalse() {
        assertFalse(vm.isSaving.value)
    }

    @Test fun initialState_keysEmpty_whenNoKeysInDb() = runBlocking {
        assertTrue(vm.keys.value.isEmpty())
    }

    @Test fun init_loadsKeysFromDb() = runBlocking {
        Repositories.keys.save(sampleKey("Loaded"))
        val freshVm = ProfileEditViewModel(ApplicationProvider.getApplicationContext())
        val list = withTimeout(3000) { freshVm.keys.first { it.isNotEmpty() } }
        assertEquals(1, list.size)
        assertEquals("Loaded", list[0].label)
    }

    @Test fun init_loadsMultipleKeys() = runBlocking {
        Repositories.keys.save(sampleKey("KeyA"))
        Repositories.keys.save(sampleKey("KeyB"))
        val freshVm = ProfileEditViewModel(ApplicationProvider.getApplicationContext())
        val list = withTimeout(3000) { freshVm.keys.first { it.size == 2 } }
        assertEquals(2, list.size)
    }

    @Test fun save_callsOnSaved() = runBlocking {
        var saved = false
        vm.save(sampleProfile()) { saved = true }
        withTimeout(3000) { vm.isSaving.first { !it } }
        assertTrue(saved)
    }

    @Test fun save_persistsProfileToDb() = runBlocking {
        vm.save(sampleProfile()) {}
        withTimeout(3000) { vm.isSaving.first { !it } }
        val all = Repositories.profiles.getAll()
        assertTrue(all.any { it.label == "Prod" })
    }

    @Test fun save_isSavingReturnsFalseAfterCompletion() = runBlocking {
        vm.save(sampleProfile()) {}
        withTimeout(3000) { vm.isSaving.first { !it } }
        assertFalse(vm.isSaving.value)
    }

    @Test fun save_calledTwiceConcurrently_onlyOneSaves() = runBlocking {
        var callCount = 0
        vm.save(sampleProfile()) { callCount++ }
        vm.save(sampleProfile()) { callCount++ }
        withTimeout(3000) { vm.isSaving.first { !it } }
        assertEquals(1, callCount)
        val all = Repositories.profiles.getAll()
        assertEquals(1, all.size)
    }
}
