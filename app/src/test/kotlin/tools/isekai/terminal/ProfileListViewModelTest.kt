package tools.isekai.terminal

import android.app.Application
import androidx.test.core.app.ApplicationProvider
import tools.isekai.terminal.data.ConnectionProfile
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
import org.junit.Assert.assertNotNull
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
class ProfileListViewModelTest {
    private lateinit var vm: ProfileListViewModel

    @Before fun setup() {
        Dispatchers.setMain(UnconfinedTestDispatcher())
        val app = ApplicationProvider.getApplicationContext<Application>()
        Repositories.init(app)
        runBlocking {
            Repositories.profiles.getAll().forEach { Repositories.profiles.delete(it) }
        }
        vm = ProfileListViewModel(app)
    }

    @After fun teardown() { Dispatchers.resetMain() }

    private fun profile(label: String) = ConnectionProfile(
        label = label, host = "example.com", username = "user", authType = "password",
    )

    private suspend fun awaitProfiles(condition: (List<ConnectionProfile>) -> Boolean) =
        withTimeout(3000) { vm.profiles.first { condition(it) } }

    @Test fun initialState_profilesEmpty() = runBlocking {
        assertTrue(vm.profiles.value.isEmpty())
    }

    @Test fun loadProfiles_withData_updatesState() = runBlocking {
        Repositories.profiles.save(profile("Prod"))
        vm.loadProfiles()
        val list = awaitProfiles { it.isNotEmpty() }
        assertEquals(1, list.size)
        assertEquals("Prod", list[0].label)
    }

    @Test fun loadProfiles_multiple_returnsAll() = runBlocking {
        Repositories.profiles.save(profile("A"))
        Repositories.profiles.save(profile("B"))
        vm.loadProfiles()
        val list = awaitProfiles { it.size == 2 }
        assertEquals(2, list.size)
    }

    @Test fun requestPasswordConnect_setsPasswordTarget() {
        val p = profile("PwHost")
        vm.requestPasswordConnect(p)
        assertEquals("PwHost", vm.passwordTarget.value?.label)
    }

    @Test fun dismissPassword_clearsPasswordTarget() {
        vm.requestPasswordConnect(profile("PwHost"))
        vm.dismissPassword()
        assertNull(vm.passwordTarget.value)
    }

    @Test fun requestDelete_setsDeleteTarget() {
        val p = profile("DelHost")
        vm.requestDelete(p)
        assertEquals("DelHost", vm.deleteTarget.value?.label)
    }

    @Test fun dismissDelete_clearsDeleteTarget() {
        vm.requestDelete(profile("DelHost"))
        vm.dismissDelete()
        assertNull(vm.deleteTarget.value)
    }

    @Test fun confirmDelete_removesProfileFromDb() = runBlocking {
        val id = Repositories.profiles.save(profile("ToDelete"))
        vm.loadProfiles()
        awaitProfiles { it.isNotEmpty() }
        val toDelete = vm.profiles.value.first { it.id == id }
        vm.requestDelete(toDelete)
        vm.confirmDelete(toDelete)
        awaitProfiles { it.isEmpty() }
        assertNull(Repositories.profiles.findById(id))
        assertNull(vm.deleteTarget.value)
    }
}
