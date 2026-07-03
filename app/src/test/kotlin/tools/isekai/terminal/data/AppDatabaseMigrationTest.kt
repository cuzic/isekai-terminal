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
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Before
import org.junit.Test
import org.junit.runner.RunWith
import org.robolectric.RobolectricTestRunner
import org.robolectric.annotation.Config

/**
 * MIGRATION_8_9(スニペット)・MIGRATION_9_10(ポートフォワード)の実機検証。
 * exportSchema=false のため room-testing の MigrationTestHelper（スキーマ JSON 前提）は
 * 使わず、各バージョン時点の生スキーマを手動構築してから実際の Migration を適用し、
 * Room が結果スキーマを正しいと認識する（＝以後のクエリが通る）ことと、
 * 既存データが保持されることを確認する。
 */
@RunWith(RobolectricTestRunner::class)
@Config(sdk = [28])
class AppDatabaseMigrationTest {
    private lateinit var ctx: Application

    @Before
    fun setup() {
        ctx = ApplicationProvider.getApplicationContext()
    }

    @After
    fun teardown() {
    }

    @Test
    fun migrate8To9_addsPostConnectCommandsColumn_andSnippetsTable_preservesExistingData(): Unit = runBlocking {
        val dbName = "migration-test-8-9.db"
        ctx.deleteDatabase(dbName)

        // Arrange: v8 スキーマ(migration 1→8 適用後の最終形)の生データベースを作り、
        // 既存プロファイルを1件入れておく。
        val v8Helper = FrameworkSQLiteOpenHelperFactory().create(
            SupportSQLiteOpenHelper.Configuration.builder(ctx)
                .name(dbName)
                .callback(object : SupportSQLiteOpenHelper.Callback(8) {
                    override fun onCreate(db: SupportSQLiteDatabase) {
                        db.execSQL(
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
                                enable_upstream_failover INTEGER NOT NULL DEFAULT 0
                            )
                            """.trimIndent()
                        )
                        db.execSQL(
                            """
                            CREATE TABLE key_entries (
                                id INTEGER PRIMARY KEY AUTOINCREMENT NOT NULL,
                                label TEXT NOT NULL,
                                publicKey TEXT NOT NULL,
                                encryptedPrivateKeyPath TEXT NOT NULL,
                                kekAlias TEXT NOT NULL,
                                createdAt INTEGER NOT NULL
                            )
                            """.trimIndent()
                        )
                        db.execSQL(
                            """
                            CREATE TABLE known_hosts (
                                id INTEGER PRIMARY KEY AUTOINCREMENT NOT NULL,
                                host TEXT NOT NULL,
                                port INTEGER NOT NULL,
                                keyType TEXT NOT NULL,
                                fingerprintSha256 TEXT NOT NULL,
                                firstSeenAt INTEGER NOT NULL,
                                lastSeenAt INTEGER NOT NULL
                            )
                            """.trimIndent()
                        )
                        db.execSQL("CREATE UNIQUE INDEX index_known_hosts_host_port ON known_hosts (host, port)")
                        db.execSQL(
                            """
                            INSERT INTO connection_profiles
                                (label, host, port, username, authType, keyId, sort_order, use_tsshd, tsshd_port,
                                 transport_preference, direct_address, enable_physical_multipath,
                                 cellular_remote_address, enable_upstream_failover)
                            VALUES ('legacy', 'example.com', 22, 'user', 'password', NULL, 0, 0, 2222,
                                    'PLAIN_SSH', NULL, 0, NULL, 0)
                            """.trimIndent()
                        )
                    }

                    override fun onUpgrade(db: SupportSQLiteDatabase, oldVersion: Int, newVersion: Int) {}
                })
                .build()
        )
        v8Helper.writableDatabase // force onCreate
        v8Helper.close()

        // Act: Room を通じて実際の MIGRATION_8_9 を適用する
        val db = Room.databaseBuilder(ctx, AppDatabase::class.java, dbName)
            .addMigrations(AppDatabase.MIGRATION_8_9, AppDatabase.MIGRATION_9_10, AppDatabase.MIGRATION_10_11)
            .build()

        // Assert: 既存の行は保持され、新カラムは null
        val profiles = db.connectionProfileDao().getAll()
        assertEquals(1, profiles.size)
        assertEquals("legacy", profiles[0].label)
        assertNull(profiles[0].postConnectCommands)

        // Assert: snippets テーブルが使える（デフォルト値含む）
        assertTrue(db.snippetDao().getAll().isEmpty())
        val id = db.snippetDao().upsert(Snippet(label = "ls", command = "ls -la"))
        val saved = db.snippetDao().findById(id)!!
        assertNull(saved.profileId)
        assertTrue(saved.appendNewline)

        db.close()
        ctx.deleteDatabase(dbName)
    }

    @Test
    fun migrate9To10_addsForwardsColumn_existingRowsDefaultToEmptyList() {
        val dbName = "migration-test-9-10.db"
        ctx.deleteDatabase(dbName)

        // バージョン 9 の connection_profiles テーブル(migration 1→9 適用後の最終形)を
        // 手で作り、1 行 insert しておく。
        val factory = FrameworkSQLiteOpenHelperFactory()
        val config = SupportSQLiteOpenHelper.Configuration.builder(ctx)
            .name(dbName)
            .callback(object : SupportSQLiteOpenHelper.Callback(9) {
                override fun onCreate(db: SupportSQLiteDatabase) {
                    // AppDatabase の全エンティティ分作らないと、Room の起動時バリデーションが
                    // (今回のマイグレーションと無関係な)known_hosts / key_entries / snippets でも失敗する。
                    db.execSQL(
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
                            post_connect_commands TEXT
                        )
                        """.trimIndent()
                    )
                    db.execSQL(
                        """
                        CREATE TABLE known_hosts (
                            id INTEGER PRIMARY KEY AUTOINCREMENT NOT NULL,
                            host TEXT NOT NULL,
                            port INTEGER NOT NULL,
                            keyType TEXT NOT NULL,
                            fingerprintSha256 TEXT NOT NULL,
                            firstSeenAt INTEGER NOT NULL,
                            lastSeenAt INTEGER NOT NULL
                        )
                        """.trimIndent()
                    )
                    db.execSQL(
                        "CREATE UNIQUE INDEX index_known_hosts_host_port ON known_hosts (host, port)"
                    )
                    db.execSQL(
                        """
                        CREATE TABLE key_entries (
                            id INTEGER PRIMARY KEY AUTOINCREMENT NOT NULL,
                            label TEXT NOT NULL,
                            publicKey TEXT NOT NULL,
                            encryptedPrivateKeyPath TEXT NOT NULL,
                            kekAlias TEXT NOT NULL,
                            createdAt INTEGER NOT NULL
                        )
                        """.trimIndent()
                    )
                    db.execSQL(
                        """
                        CREATE TABLE snippets (
                            id INTEGER PRIMARY KEY AUTOINCREMENT NOT NULL,
                            label TEXT NOT NULL,
                            command TEXT NOT NULL,
                            sort_order INTEGER NOT NULL DEFAULT 0,
                            profile_id INTEGER,
                            append_newline INTEGER NOT NULL DEFAULT 1
                        )
                        """.trimIndent()
                    )
                    db.execSQL(
                        "INSERT INTO connection_profiles (label, host, username, authType) " +
                            "VALUES ('web', 'example.com', 'user', 'password')"
                    )
                }

                override fun onUpgrade(db: SupportSQLiteDatabase, oldVersion: Int, newVersion: Int) {}
            })
            .build()
        val rawHelper = factory.create(config)
        rawHelper.writableDatabase // force onCreate
        rawHelper.close()

        // Room 経由で開くと MIGRATION_9_10 が適用されるはず。
        val db = Room.databaseBuilder(ctx, AppDatabase::class.java, dbName)
            .addMigrations(AppDatabase.MIGRATION_9_10, AppDatabase.MIGRATION_10_11)
            .build()
        try {
            val profiles = runBlocking { db.connectionProfileDao().getAll() }
            assertEquals(1, profiles.size)
            assertEquals("web", profiles[0].label)
            assertTrue("既存行の forwards は空リストであるべき", profiles[0].forwards.isEmpty())
        } finally {
            db.close()
            ctx.deleteDatabase(dbName)
        }
    }
}
