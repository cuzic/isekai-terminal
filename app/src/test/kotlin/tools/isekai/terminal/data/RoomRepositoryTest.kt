package tools.isekai.terminal.data

import android.app.Application
import androidx.room.Room
import androidx.test.core.app.ApplicationProvider
import kotlinx.coroutines.runBlocking
import org.junit.After
import org.junit.Assert.assertEquals
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Before
import org.junit.Test
import org.junit.runner.RunWith
import org.robolectric.RobolectricTestRunner
import org.robolectric.annotation.Config

@RunWith(RobolectricTestRunner::class)
@Config(sdk = [28])
class ConnectionProfileRepositoryTest {
    private lateinit var db: AppDatabase
    private lateinit var repo: ConnectionProfileRepository

    @Before fun setup() {
        val ctx = ApplicationProvider.getApplicationContext<Application>()
        db = Room.inMemoryDatabaseBuilder(ctx, AppDatabase::class.java)
            .allowMainThreadQueries().build()
        repo = ConnectionProfileRepository(db.connectionProfileDao())
    }

    @After fun teardown() { db.close() }

    private fun profile(label: String, sortOrder: Int = 0) = ConnectionProfile(
        label = label, host = "example.com", username = "user",
        authType = "password", sortOrder = sortOrder,
    )

    @Test fun save_and_getAll_returnsProfile() = runBlocking {
        repo.save(profile("web"))
        val all = repo.getAll()
        assertEquals(1, all.size)
        assertEquals("web", all[0].label)
    }

    @Test fun save_and_findById_returnsProfile() = runBlocking {
        val id = repo.save(profile("web"))
        val found = repo.findById(id)
        assertEquals("web", found?.label)
        assertEquals(id, found?.id)
    }

    @Test fun findById_nonexistent_returnsNull() = runBlocking {
        assertNull(repo.findById(999))
    }

    @Test fun save_multiple_sortedByLabelThenSortOrder() = runBlocking {
        repo.save(profile("charlie", sortOrder = 1))
        repo.save(profile("alpha", sortOrder = 1))
        repo.save(profile("bravo", sortOrder = 0))
        val labels = repo.getAll().map { it.label }
        assertEquals(listOf("bravo", "alpha", "charlie"), labels)
    }

    @Test fun update_via_upsert_replacesExisting() = runBlocking {
        val id = repo.save(profile("original"))
        val stored = repo.findById(id)!!
        repo.save(stored.copy(label = "renamed"))
        val all = repo.getAll()
        assertEquals(1, all.size)
        assertEquals("renamed", all[0].label)
        assertEquals(id, all[0].id)
    }

    @Test fun delete_removesFromDb() = runBlocking {
        val id = repo.save(profile("web"))
        repo.delete(repo.findById(id)!!)
        assertTrue(repo.getAll().isEmpty())
        assertNull(repo.findById(id))
    }

    @Test fun getAll_emptyDb_returnsEmpty() = runBlocking {
        assertTrue(repo.getAll().isEmpty())
    }
}

@RunWith(RobolectricTestRunner::class)
@Config(sdk = [28])
class KeyEntryRepositoryTest {
    private lateinit var db: AppDatabase
    private lateinit var repo: KeyEntryRepository

    @Before fun setup() {
        val ctx = ApplicationProvider.getApplicationContext<Application>()
        db = Room.inMemoryDatabaseBuilder(ctx, AppDatabase::class.java)
            .allowMainThreadQueries().build()
        repo = KeyEntryRepository(db.keyEntryDao())
    }

    @After fun teardown() { db.close() }

    private fun key(label: String) = KeyEntry(
        label = label, publicKey = "ssh-ed25519 AAAA$label",
        encryptedPrivateKeyPath = "/keys/$label.enc",
        kekAlias = "kek_$label", createdAt = 1_000L,
    )

    @Test fun save_and_findById() = runBlocking {
        val id = repo.save(key("deploy"))
        val found = repo.findById(id)
        assertEquals("deploy", found?.label)
        assertEquals(id, found?.id)
    }

    @Test fun delete_removesKey() = runBlocking {
        val id = repo.save(key("deploy"))
        repo.delete(repo.findById(id)!!)
        assertTrue(repo.getAll().isEmpty())
        assertNull(repo.findById(id))
    }

    @Test fun getAll_returnsAllKeys() = runBlocking {
        repo.save(key("bravo"))
        repo.save(key("alpha"))
        val labels = repo.getAll().map { it.label }
        assertEquals(listOf("alpha", "bravo"), labels)
    }
}

@RunWith(RobolectricTestRunner::class)
@Config(sdk = [28])
class KnownHostRepositoryTest {
    private lateinit var db: AppDatabase
    private lateinit var repo: KnownHostRepository

    @Before fun setup() {
        val ctx = ApplicationProvider.getApplicationContext<Application>()
        db = Room.inMemoryDatabaseBuilder(ctx, AppDatabase::class.java)
            .allowMainThreadQueries().build()
        repo = KnownHostRepository(db.knownHostDao())
    }

    @After fun teardown() { db.close() }

    @Test fun verify_unknownHost_returnsUnknown() = runBlocking {
        val status = repo.verify("example.com", 22, "fp-aaa")
        assertEquals(HostKeyStatus.Unknown, status)
    }

    @Test fun trust_then_verify_returnsTrusted() = runBlocking {
        repo.trust("example.com", 22, "ssh-ed25519", "fp-aaa")
        val status = repo.verify("example.com", 22, "fp-aaa")
        assertEquals(HostKeyStatus.Trusted, status)
    }

    @Test fun trust_then_verify_differentFingerprint_returnsChanged() = runBlocking {
        repo.trust("example.com", 22, "ssh-ed25519", "fp-aaa")
        val status = repo.verify("example.com", 22, "fp-bbb")
        assertTrue(status is HostKeyStatus.Changed)
    }

    @Test fun changed_includesOldFingerprint() = runBlocking {
        repo.trust("example.com", 22, "ssh-ed25519", "fp-aaa")
        val status = repo.verify("example.com", 22, "fp-bbb")
        assertEquals("fp-aaa", (status as HostKeyStatus.Changed).oldFingerprint)
    }

    @Test fun forget_removesEntry() = runBlocking {
        repo.trust("example.com", 22, "ssh-ed25519", "fp-aaa")
        repo.forget("example.com", 22)
        assertEquals(HostKeyStatus.Unknown, repo.verify("example.com", 22, "fp-aaa"))
    }

    @Test fun trust_idempotent_updatesLastSeen() = runBlocking {
        repo.trust("example.com", 22, "ssh-ed25519", "fp-aaa")
        val first = db.knownHostDao().findByHostPort("example.com", 22)!!
        Thread.sleep(5)
        repo.trust("example.com", 22, "ssh-ed25519", "fp-aaa")
        val all = db.knownHostDao().getAll()
        assertEquals(1, all.size)
        assertEquals(HostKeyStatus.Trusted, repo.verify("example.com", 22, "fp-aaa"))
        val second = db.knownHostDao().findByHostPort("example.com", 22)!!
        assertEquals(first.firstSeenAt, second.firstSeenAt)
        assertTrue(second.lastSeenAt >= first.lastSeenAt)
    }
}
