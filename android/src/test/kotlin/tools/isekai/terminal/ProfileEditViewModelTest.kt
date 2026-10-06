package tools.isekai.terminal

import android.app.Application
import androidx.test.core.app.ApplicationProvider
import tools.isekai.terminal.data.ConnectionProfile
import tools.isekai.terminal.data.KeyEntry
import tools.isekai.terminal.data.Repositories
import java.util.concurrent.atomic.AtomicInteger
import kotlinx.coroutines.CompletableDeferred
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

    // [SavingEditViewModel.save]は`_isSaving.value = false`を**先に**書いてから`onSaved()`を
    // 呼ぶ。テスト用Mainディスパッチャ(UnconfinedTestDispatcher)上ではwithContext(IO)から
    // 戻った継続がIOスレッド上でそのまま走るため、`isSaving.first { !it }`で待つと
    // runBlockingスレッドが`onSaved()`の実行前に起きてしまい、コールバックの副作用を
    // 読み損ねる競合があった(CI 2026-10-06 run 37440961829で実際に
    // save_calledTwiceConcurrently_onlyOneSavesが失敗)。onSaved()の完了自体を
    // CompletableDeferredで待つ。persist()はonSaved()より前に完了しているので、
    // その後のDB読み出しも決定的に見える。
    @Test fun save_callsOnSaved() = runBlocking {
        val saved = CompletableDeferred<Unit>()
        vm.save(sampleProfile()) { saved.complete(Unit) }
        withTimeout(3000) { saved.await() }
        assertTrue(saved.isCompleted)
        assertFalse(vm.isSaving.value)
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
        // 2回目のsave()は1回目が立てたisSavingフラグを見て同期的にreturnする
        // (launchすらしない)ので、1回目のonSaved()完了を待てば2回目のコールバックが
        // 後から届く余地は無い。
        val callCount = AtomicInteger(0)
        val firstSaved = CompletableDeferred<Unit>()
        vm.save(sampleProfile()) { callCount.incrementAndGet(); firstSaved.complete(Unit) }
        vm.save(sampleProfile()) { callCount.incrementAndGet(); firstSaved.complete(Unit) }
        withTimeout(3000) { firstSaved.await() }
        assertEquals(1, callCount.get())
        val all = Repositories.profiles.getAll()
        assertEquals(1, all.size)
    }
}
