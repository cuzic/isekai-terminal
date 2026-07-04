package tools.isekai.terminal.data

import android.app.Application
import androidx.room.Room
import androidx.sqlite.db.SupportSQLiteDatabase
import androidx.sqlite.db.SupportSQLiteOpenHelper
import androidx.sqlite.db.framework.FrameworkSQLiteOpenHelperFactory
import androidx.test.core.app.ApplicationProvider
import kotlinx.coroutines.runBlocking
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

    @Test fun save_and_findById_roundtripsDirectAddress() = runBlocking {
        val id = repo.save(profile("web").copy(directAddress = "203.0.113.5"))
        assertEquals("203.0.113.5", repo.findById(id)?.directAddress)
    }

    @Test fun directAddress_defaultsToNull() = runBlocking {
        val id = repo.save(profile("web"))
        assertNull(repo.findById(id)?.directAddress)
    }

    @Test fun enablePhysicalMultipath_defaultsToFalse() = runBlocking {
        val id = repo.save(profile("web"))
        assertEquals(false, repo.findById(id)?.enablePhysicalMultipath)
    }

    @Test fun save_and_findById_roundtripsEnablePhysicalMultipath() = runBlocking {
        val id = repo.save(profile("web").copy(enablePhysicalMultipath = true))
        assertEquals(true, repo.findById(id)?.enablePhysicalMultipath)
    }

    // ── Phase 10: STUN+SSHランデブー方式のP2P ─────────────────────────

    @Test fun stunServer_defaultsToNull() = runBlocking {
        val id = repo.save(profile("web"))
        assertNull(repo.findById(id)?.stunServer)
    }

    @Test fun save_and_findById_roundtripsStunServer() = runBlocking {
        val id = repo.save(profile("web").copy(stunServer = "stun.example.com:3478"))
        assertEquals("stun.example.com:3478", repo.findById(id)?.stunServer)
    }

    // ── Phase 10: MASQUE relay経由のP2P ────────────────────────────────

    @Test fun relayFields_defaultToNull() = runBlocking {
        val id = repo.save(profile("web"))
        val found = repo.findById(id)
        assertNull(found?.relayAddr)
        assertNull(found?.relaySni)
        assertNull(found?.relayJwt)
    }

    @Test fun save_and_findById_roundtripsRelayFields() = runBlocking {
        val id = repo.save(
            profile("web").copy(
                relayAddr = "relay.example.com:443",
                relaySni = "relay.example.com",
                relayJwt = "eyJhbGciOiJSUzI1NiJ9.test.sig",
            )
        )
        val found = repo.findById(id)
        assertEquals("relay.example.com:443", found?.relayAddr)
        assertEquals("relay.example.com", found?.relaySni)
        assertEquals("eyJhbGciOiJSUzI1NiJ9.test.sig", found?.relayJwt)
    }

    @Test fun hasRelayConfig_falseWhenAnyFieldMissing() = runBlocking {
        assertFalse(profile("web").hasRelayConfig)
        assertFalse(profile("web").copy(relayAddr = "relay.example.com:443").hasRelayConfig)
        assertFalse(
            profile("web").copy(
                relayAddr = "relay.example.com:443", relaySni = "relay.example.com",
            ).hasRelayConfig
        )
    }

    @Test fun hasRelayConfig_trueWhenAllThreeFieldsSet() = runBlocking {
        val complete = profile("web").copy(
            relayAddr = "relay.example.com:443",
            relaySni = "relay.example.com",
            relayJwt = "eyJhbGciOiJSUzI1NiJ9.test.sig",
        )
        assertTrue(complete.hasRelayConfig)
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
class SnippetRepositoryTest {
    private lateinit var db: AppDatabase
    private lateinit var profileRepo: ConnectionProfileRepository
    private lateinit var repo: SnippetRepository

    @Before fun setup() {
        val ctx = ApplicationProvider.getApplicationContext<Application>()
        db = Room.inMemoryDatabaseBuilder(ctx, AppDatabase::class.java)
            .allowMainThreadQueries().build()
        profileRepo = ConnectionProfileRepository(db.connectionProfileDao())
        repo = SnippetRepository(db.snippetDao())
    }

    @After fun teardown() { db.close() }

    private fun snippet(label: String, profileId: Long? = null, sortOrder: Int = 0) = Snippet(
        label = label, command = "echo $label", profileId = profileId, sortOrder = sortOrder,
    )

    @Test fun save_and_getAll_returnsSnippet() = runBlocking {
        repo.save(snippet("ll"))
        val all = repo.getAll()
        assertEquals(1, all.size)
        assertEquals("ll", all[0].label)
    }

    @Test fun save_and_findById_returnsSnippet() = runBlocking {
        val id = repo.save(snippet("ll"))
        val found = repo.findById(id)
        assertEquals("ll", found?.label)
        assertEquals(id, found?.id)
    }

    @Test fun findById_nonexistent_returnsNull() = runBlocking {
        assertNull(repo.findById(999))
    }

    @Test fun defaultValues_appendNewlineTrue_profileIdNull() = runBlocking {
        val id = repo.save(snippet("ll"))
        val found = repo.findById(id)!!
        assertTrue(found.appendNewline)
        assertNull(found.profileId)
    }

    @Test fun update_via_upsert_replacesExisting() = runBlocking {
        val id = repo.save(snippet("original"))
        val stored = repo.findById(id)!!
        repo.save(stored.copy(label = "renamed"))
        val all = repo.getAll()
        assertEquals(1, all.size)
        assertEquals("renamed", all[0].label)
        assertEquals(id, all[0].id)
    }

    @Test fun delete_removesFromDb() = runBlocking {
        val id = repo.save(snippet("ll"))
        repo.delete(repo.findById(id)!!)
        assertTrue(repo.getAll().isEmpty())
        assertNull(repo.findById(id))
    }

    @Test fun getAll_emptyDb_returnsEmpty() = runBlocking {
        assertTrue(repo.getAll().isEmpty())
    }

    // ── merge ロジック（共通 + プロファイル専用）────────────────────

    @Test fun getForProfile_returnsOnlyCommonWhenNoProfileSpecific() = runBlocking {
        val profileId = profileRepo.save(
            ConnectionProfile(label = "web", host = "h", username = "u", authType = "password")
        )
        repo.save(snippet("common", profileId = null))
        val result = repo.getForProfile(profileId)
        assertEquals(listOf("common"), result.map { it.label })
    }

    @Test fun getForProfile_mergesCommonAndProfileSpecific() = runBlocking {
        val profileId = profileRepo.save(
            ConnectionProfile(label = "web", host = "h", username = "u", authType = "password")
        )
        val otherProfileId = profileRepo.save(
            ConnectionProfile(label = "db", host = "h2", username = "u", authType = "password")
        )
        repo.save(snippet("common", profileId = null))
        repo.save(snippet("web-only", profileId = profileId))
        repo.save(snippet("db-only", profileId = otherProfileId))

        val result = repo.getForProfile(profileId).map { it.label }.toSet()
        assertEquals(setOf("common", "web-only"), result)
    }

    @Test fun getForProfile_excludesOtherProfilesSnippets() = runBlocking {
        val profileId = profileRepo.save(
            ConnectionProfile(label = "web", host = "h", username = "u", authType = "password")
        )
        val otherProfileId = profileRepo.save(
            ConnectionProfile(label = "db", host = "h2", username = "u", authType = "password")
        )
        repo.save(snippet("db-only", profileId = otherProfileId))

        val result = repo.getForProfile(profileId)
        assertTrue(result.none { it.label == "db-only" })
    }

    @Test fun getForProfile_nullProfileId_returnsOnlyCommonSnippets() = runBlocking {
        val profileId = profileRepo.save(
            ConnectionProfile(label = "web", host = "h", username = "u", authType = "password")
        )
        repo.save(snippet("common", profileId = null))
        repo.save(snippet("web-only", profileId = profileId))

        val result = repo.getForProfile(null)
        assertEquals(listOf("common"), result.map { it.label })
    }

    @Test fun getForProfile_orderedBySortOrderThenLabel() = runBlocking {
        val profileId = profileRepo.save(
            ConnectionProfile(label = "web", host = "h", username = "u", authType = "password")
        )
        repo.save(snippet("charlie", profileId = profileId, sortOrder = 1))
        repo.save(snippet("alpha", profileId = profileId, sortOrder = 1))
        repo.save(snippet("bravo", profileId = profileId, sortOrder = 0))

        val labels = repo.getForProfile(profileId).map { it.label }
        assertEquals(listOf("bravo", "alpha", "charlie"), labels)
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

/**
 * SSH agent forwarding 追加に伴う Room マイグレーション (v3 → v4) のテスト。
 * `exportSchema = false` のためスキーマ json 資産が無く `MigrationTestHelper` を使えないので、
 * v3 時点のテーブルを手動で構築 → `AppDatabase.MIGRATION_3_4` 込みで開き直す形で検証する。
 */
@RunWith(RobolectricTestRunner::class)
@Config(sdk = [28])
class AppDatabaseMigration3To4Test {
    private lateinit var ctx: Application
    private val dbName = "migration-test-3-4.db"

    @Before fun setup() {
        ctx = ApplicationProvider.getApplicationContext()
        ctx.deleteDatabase(dbName)
    }

    @After fun teardown() {
        ctx.deleteDatabase(dbName)
    }

    /**
     * v10 時点(migration 1→10 適用後の最終形、`enable_agent_forward` 列追加前)の
     * データベースを再現する。`known_hosts` / `key_entries` / `snippets` は Room 自身に
     * 現行（v14）スキーマ一式を作らせてそのまま使い（手書き DDL の食い違いリスクを避ける）、
     * `connection_profiles` テーブルだけを v10 の形に手動で作り直したうえで
     * `user_version` を 10 に戻す。
     */
    private fun createV10Database() {
        Room.databaseBuilder(ctx, AppDatabase::class.java, dbName).build().apply {
            openHelper.writableDatabase // force file creation at the current version
            close()
        }

        val helper = FrameworkSQLiteOpenHelperFactory().create(
            SupportSQLiteOpenHelper.Configuration.builder(ctx)
                .name(dbName)
                // このコールバックの宣言バージョンは、直前の Room ビルドが作った実ファイルの
                // user_version（＝AppDatabase の現行 version）と一致させること。ずれると
                // SQLiteOpenHelper のデフォルト onDowngrade（例外送出）が発火してしまう。
                .callback(object : SupportSQLiteOpenHelper.Callback(17) {
                    override fun onCreate(db: SupportSQLiteDatabase) {}
                    override fun onUpgrade(db: SupportSQLiteDatabase, oldVersion: Int, newVersion: Int) {}
                })
                .build()
        )
        helper.writableDatabase.apply {
            execSQL("DROP TABLE connection_profiles")
            execSQL(
                """
                CREATE TABLE connection_profiles (
                    id INTEGER PRIMARY KEY AUTOINCREMENT NOT NULL,
                    label TEXT NOT NULL,
                    host TEXT NOT NULL,
                    port INTEGER NOT NULL DEFAULT 22,
                    username TEXT NOT NULL,
                    authType TEXT NOT NULL,
                    keyId INTEGER,
                    sort_order INTEGER NOT NULL DEFAULT 0,
                    use_tsshd INTEGER NOT NULL DEFAULT 0,
                    tsshd_port INTEGER NOT NULL DEFAULT 2222,
                    transport_preference TEXT NOT NULL DEFAULT 'PLAIN_SSH',
                    direct_address TEXT,
                    enable_physical_multipath INTEGER NOT NULL DEFAULT 0,
                    cellular_remote_address TEXT,
                    enable_upstream_failover INTEGER NOT NULL DEFAULT 0,
                    post_connect_commands TEXT,
                    forwards TEXT NOT NULL DEFAULT '[]'
                )
                """.trimIndent()
            )
            execSQL(
                "INSERT INTO connection_profiles (label, host, username, authType) " +
                    "VALUES ('web', 'example.com', 'user', 'password')"
            )
            execSQL("PRAGMA user_version = 10")
            close()
        }
    }

    @Test
    fun migrate10To11_addsColumn_existingRowDefaultsToDisabled() {
        createV10Database()

        val db = Room.databaseBuilder(ctx, AppDatabase::class.java, dbName)
            .addMigrations(AppDatabase.MIGRATION_10_11, AppDatabase.MIGRATION_11_12, AppDatabase.MIGRATION_12_13, AppDatabase.MIGRATION_13_14, AppDatabase.MIGRATION_14_15, AppDatabase.MIGRATION_15_16, AppDatabase.MIGRATION_16_17)
            .build()
        try {
            val profiles = runBlocking { db.connectionProfileDao().getAll() }
            assertEquals(1, profiles.size)
            assertEquals("web", profiles[0].label)
            assertFalse(profiles[0].enableAgentForward)
        } finally {
            db.close()
        }
    }

    @Test
    fun migrate10To11_thenSavingWithAgentForwardEnabled_persists() {
        createV10Database()

        val db = Room.databaseBuilder(ctx, AppDatabase::class.java, dbName)
            .addMigrations(AppDatabase.MIGRATION_10_11, AppDatabase.MIGRATION_11_12, AppDatabase.MIGRATION_12_13, AppDatabase.MIGRATION_13_14, AppDatabase.MIGRATION_14_15, AppDatabase.MIGRATION_15_16, AppDatabase.MIGRATION_16_17)
            .build()
        try {
            val dao = db.connectionProfileDao()
            val existing = runBlocking { dao.getAll() }.single()
            runBlocking { dao.upsert(existing.copy(enableAgentForward = true)) }

            val updated = runBlocking { dao.findById(existing.id) }
            assertTrue(updated!!.enableAgentForward)
        } finally {
            db.close()
        }
    }
}

/**
 * Phase 10: STUN+SSHランデブー方式のP2P用`stun_server`列追加(v12→v13)・MASQUE relay経由の
 * P2P用`relay_addr`/`relay_sni`/`relay_jwt`列追加(v13→v14)のRoomマイグレーションのテスト。
 * `AppDatabaseMigration3To4Test`と同じ手法(v12/v13時点のテーブルを手動で構築→対象migration
 * 込みで開き直す)を踏襲する。
 */
@RunWith(RobolectricTestRunner::class)
@Config(sdk = [28])
class AppDatabaseMigration12To14Test {
    private lateinit var ctx: Application
    private val dbName = "migration-test-12-14.db"

    @Before fun setup() {
        ctx = ApplicationProvider.getApplicationContext()
        ctx.deleteDatabase(dbName)
    }

    @After fun teardown() {
        ctx.deleteDatabase(dbName)
    }

    /** v12時点(migration 1→12適用後の最終形、`stun_server`列追加前)のデータベースを再現する。 */
    private fun createV12Database() {
        Room.databaseBuilder(ctx, AppDatabase::class.java, dbName).build().apply {
            openHelper.writableDatabase // force file creation at the current version
            close()
        }

        val helper = FrameworkSQLiteOpenHelperFactory().create(
            SupportSQLiteOpenHelper.Configuration.builder(ctx)
                .name(dbName)
                // このコールバックの宣言バージョンは、直前の Room ビルドが作った実ファイルの
                // user_version（＝AppDatabase の現行 version）と一致させること。ずれると
                // SQLiteOpenHelper のデフォルト onDowngrade（例外送出）が発火してしまう。
                .callback(object : SupportSQLiteOpenHelper.Callback(17) {
                    override fun onCreate(db: SupportSQLiteDatabase) {}
                    override fun onUpgrade(db: SupportSQLiteDatabase, oldVersion: Int, newVersion: Int) {}
                })
                .build()
        )
        helper.writableDatabase.apply {
            execSQL("DROP TABLE connection_profiles")
            execSQL(
                """
                CREATE TABLE connection_profiles (
                    id INTEGER PRIMARY KEY AUTOINCREMENT NOT NULL,
                    label TEXT NOT NULL,
                    host TEXT NOT NULL,
                    port INTEGER NOT NULL DEFAULT 22,
                    username TEXT NOT NULL,
                    authType TEXT NOT NULL,
                    keyId INTEGER,
                    sort_order INTEGER NOT NULL DEFAULT 0,
                    use_tsshd INTEGER NOT NULL DEFAULT 0,
                    tsshd_port INTEGER NOT NULL DEFAULT 2222,
                    transport_preference TEXT NOT NULL DEFAULT 'PLAIN_SSH',
                    direct_address TEXT,
                    enable_physical_multipath INTEGER NOT NULL DEFAULT 0,
                    cellular_remote_address TEXT,
                    enable_upstream_failover INTEGER NOT NULL DEFAULT 0,
                    post_connect_commands TEXT,
                    forwards TEXT NOT NULL DEFAULT '[]',
                    enable_agent_forward INTEGER NOT NULL DEFAULT 0,
                    jump_host TEXT,
                    jump_port INTEGER NOT NULL DEFAULT 22,
                    jump_username TEXT,
                    jump_auth_type TEXT,
                    jump_key_id INTEGER
                )
                """.trimIndent()
            )
            execSQL(
                "INSERT INTO connection_profiles (label, host, username, authType) " +
                    "VALUES ('web', 'example.com', 'user', 'password')"
            )
            execSQL("PRAGMA user_version = 12")
            close()
        }
    }

    @Test
    fun migrate12To13_addsStunServerColumn_existingRowDefaultsToNull() {
        createV12Database()

        val db = Room.databaseBuilder(ctx, AppDatabase::class.java, dbName)
            .addMigrations(AppDatabase.MIGRATION_12_13, AppDatabase.MIGRATION_13_14, AppDatabase.MIGRATION_14_15, AppDatabase.MIGRATION_15_16, AppDatabase.MIGRATION_16_17)
            .build()
        try {
            val profiles = runBlocking { db.connectionProfileDao().getAll() }
            assertEquals(1, profiles.size)
            assertEquals("web", profiles[0].label)
            assertNull(profiles[0].stunServer)
        } finally {
            db.close()
        }
    }

    /** v13時点(migration 1→13適用後の最終形、relay列追加前)のデータベースを再現する。
     *  既存行には`stun_server`に非null値を入れておき、13→14マイグレーションが
     *  他の既存列を壊さないことも合わせて確認できるようにする。 */
    private fun createV13Database() {
        Room.databaseBuilder(ctx, AppDatabase::class.java, dbName).build().apply {
            openHelper.writableDatabase
            close()
        }

        val helper = FrameworkSQLiteOpenHelperFactory().create(
            SupportSQLiteOpenHelper.Configuration.builder(ctx)
                .name(dbName)
                .callback(object : SupportSQLiteOpenHelper.Callback(17) {
                    override fun onCreate(db: SupportSQLiteDatabase) {}
                    override fun onUpgrade(db: SupportSQLiteDatabase, oldVersion: Int, newVersion: Int) {}
                })
                .build()
        )
        helper.writableDatabase.apply {
            execSQL("DROP TABLE connection_profiles")
            execSQL(
                """
                CREATE TABLE connection_profiles (
                    id INTEGER PRIMARY KEY AUTOINCREMENT NOT NULL,
                    label TEXT NOT NULL,
                    host TEXT NOT NULL,
                    port INTEGER NOT NULL DEFAULT 22,
                    username TEXT NOT NULL,
                    authType TEXT NOT NULL,
                    keyId INTEGER,
                    sort_order INTEGER NOT NULL DEFAULT 0,
                    use_tsshd INTEGER NOT NULL DEFAULT 0,
                    tsshd_port INTEGER NOT NULL DEFAULT 2222,
                    transport_preference TEXT NOT NULL DEFAULT 'PLAIN_SSH',
                    direct_address TEXT,
                    enable_physical_multipath INTEGER NOT NULL DEFAULT 0,
                    cellular_remote_address TEXT,
                    enable_upstream_failover INTEGER NOT NULL DEFAULT 0,
                    post_connect_commands TEXT,
                    forwards TEXT NOT NULL DEFAULT '[]',
                    enable_agent_forward INTEGER NOT NULL DEFAULT 0,
                    jump_host TEXT,
                    jump_port INTEGER NOT NULL DEFAULT 22,
                    jump_username TEXT,
                    jump_auth_type TEXT,
                    jump_key_id INTEGER,
                    stun_server TEXT
                )
                """.trimIndent()
            )
            execSQL(
                "INSERT INTO connection_profiles (label, host, username, authType, stun_server) " +
                    "VALUES ('web', 'example.com', 'user', 'password', 'stun.example.com:3478')"
            )
            execSQL("PRAGMA user_version = 13")
            close()
        }
    }

    @Test
    fun migrate13To14_addsRelayColumns_existingRowDefaultsToNull_preservesStunServer() {
        createV13Database()

        val db = Room.databaseBuilder(ctx, AppDatabase::class.java, dbName)
            .addMigrations(AppDatabase.MIGRATION_13_14, AppDatabase.MIGRATION_14_15, AppDatabase.MIGRATION_15_16, AppDatabase.MIGRATION_16_17)
            .build()
        try {
            val profiles = runBlocking { db.connectionProfileDao().getAll() }
            assertEquals(1, profiles.size)
            assertEquals("web", profiles[0].label)
            assertEquals("stun.example.com:3478", profiles[0].stunServer)
            assertNull(profiles[0].relayAddr)
            assertNull(profiles[0].relaySni)
            assertNull(profiles[0].relayJwt)
        } finally {
            db.close()
        }
    }
}
