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
 * MIGRATION_3_4 の実機検証。exportSchema=false のため room-testing の
 * MigrationTestHelper（スキーマ JSON 前提）は使わず、v3 時点の生スキーマを手動構築してから
 * 実際の Migration を適用し、Room が結果スキーマを正しいと認識する（＝以後のクエリが通る）
 * ことと、既存データが保持されることを確認する。
 */
@RunWith(RobolectricTestRunner::class)
@Config(sdk = [28])
class AppDatabaseMigrationTest {
    private val dbName = "migration-test-3-4.db"
    private lateinit var ctx: Application

    @Before
    fun setup() {
        ctx = ApplicationProvider.getApplicationContext()
        ctx.deleteDatabase(dbName)
    }

    @After
    fun teardown() {
        ctx.deleteDatabase(dbName)
    }

    @Test
    fun migrate3To4_addsPostConnectCommandsColumn_andSnippetsTable_preservesExistingData() = runBlocking {
        // Arrange: v3 スキーマの生データベースを作り、既存プロファイルを1件入れておく
        val v3Helper = FrameworkSQLiteOpenHelperFactory().create(
            SupportSQLiteOpenHelper.Configuration.builder(ctx)
                .name(dbName)
                .callback(object : SupportSQLiteOpenHelper.Callback(3) {
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
                                tsshd_port INTEGER NOT NULL DEFAULT 2222
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
                                (label, host, port, username, authType, keyId, sort_order, use_tsshd, tsshd_port)
                            VALUES ('legacy', 'example.com', 22, 'user', 'password', NULL, 0, 0, 2222)
                            """.trimIndent()
                        )
                    }

                    override fun onUpgrade(db: SupportSQLiteDatabase, oldVersion: Int, newVersion: Int) {}
                })
                .build()
        )
        v3Helper.writableDatabase // force onCreate
        v3Helper.close()

        // Act: Room を通じて実際の MIGRATION_3_4 を適用する
        val db = Room.databaseBuilder(ctx, AppDatabase::class.java, dbName)
            .addMigrations(AppDatabase.MIGRATION_3_4)
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
    }
}
