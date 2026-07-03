package tools.isekai.terminal.data

import android.content.Context
import androidx.room.Database
import androidx.room.Room
import androidx.room.RoomDatabase
import androidx.room.TypeConverters
import androidx.room.migration.Migration
import androidx.sqlite.db.SupportSQLiteDatabase

@Database(
    entities = [KnownHost::class, ConnectionProfile::class, KeyEntry::class, Snippet::class],
    version = 10,
    exportSchema = false,
)
@TypeConverters(PortForwardListConverter::class)
abstract class AppDatabase : RoomDatabase() {
    abstract fun knownHostDao(): KnownHostDao
    abstract fun connectionProfileDao(): ConnectionProfileDao
    abstract fun keyEntryDao(): KeyEntryDao
    abstract fun snippetDao(): SnippetDao

    companion object {
        @Volatile private var instance: AppDatabase? = null

        private val MIGRATION_1_2 = object : Migration(1, 2) {
            override fun migrate(db: SupportSQLiteDatabase) {
                db.execSQL("ALTER TABLE connection_profiles ADD COLUMN use_tsshd INTEGER NOT NULL DEFAULT 0")
                db.execSQL("ALTER TABLE connection_profiles ADD COLUMN tsshd_port INTEGER NOT NULL DEFAULT 2222")
            }
        }

        private val MIGRATION_2_3 = object : Migration(2, 3) {
            override fun migrate(db: SupportSQLiteDatabase) {
                db.execSQL("""
                    CREATE TABLE IF NOT EXISTS connection_profiles_new (
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
                """.trimIndent())
                db.execSQL("""
                    INSERT INTO connection_profiles_new
                        (id, label, host, port, username, authType, keyId, sort_order, use_tsshd, tsshd_port)
                    SELECT id, label, host, port, username, authType, keyId, sort_order, use_tsshd, tsshd_port
                    FROM connection_profiles
                """.trimIndent())
                db.execSQL("DROP TABLE connection_profiles")
                db.execSQL("ALTER TABLE connection_profiles_new RENAME TO connection_profiles")
            }
        }

        private val MIGRATION_3_4 = object : Migration(3, 4) {
            override fun migrate(db: SupportSQLiteDatabase) {
                // Phase 7: TransportPreference を導入。既存 use_tsshd の値を引き継いで
                // 挙動を変えないようにする（true→TSSHD_QUIC、false→PLAIN_SSH）。
                db.execSQL("ALTER TABLE connection_profiles ADD COLUMN transport_preference TEXT NOT NULL DEFAULT 'PLAIN_SSH'")
                db.execSQL("UPDATE connection_profiles SET transport_preference = 'TSSHD_QUIC' WHERE use_tsshd = 1")
            }
        }

        private val MIGRATION_4_5 = object : Migration(4, 5) {
            override fun migrate(db: SupportSQLiteDatabase) {
                // Phase 9: Tailscale⇔直接アドレスの受動的マルチパスフェイルオーバー用の
                // 第2アドレス（path1）。未設定なら path0 のみで動く（IsekaiHelperQuic 相当）。
                db.execSQL("ALTER TABLE connection_profiles ADD COLUMN direct_address TEXT")
            }
        }

        private val MIGRATION_5_6 = object : Migration(5, 6) {
            override fun migrate(db: SupportSQLiteDatabase) {
                // Phase 9-4（実験的機能、既定OFF）: Wi-Fi/セルラー物理無線への同時マルチパスも
                // 試すかどうかのオプトイン。
                db.execSQL("ALTER TABLE connection_profiles ADD COLUMN enable_physical_multipath INTEGER NOT NULL DEFAULT 0")
            }
        }

        private val MIGRATION_6_7 = object : Migration(6, 7) {
            override fun migrate(db: SupportSQLiteDatabase) {
                // Phase 9-4追加検証（実験的機能）: セルラー物理path候補用の別リモートアドレス
                // （IPv6等）。未設定ならdirect_addressと同じアドレスを使う。
                db.execSQL("ALTER TABLE connection_profiles ADD COLUMN cellular_remote_address TEXT")
            }
        }

        private val MIGRATION_7_8 = object : Migration(7, 8) {
            override fun migrate(db: SupportSQLiteDatabase) {
                // 「WiFiは繋がっているがupstreamが死んでいる」を検知したらセルラーへ
                // rebindする機能（実験的、既定OFF）のオプトイン。
                db.execSQL("ALTER TABLE connection_profiles ADD COLUMN enable_upstream_failover INTEGER NOT NULL DEFAULT 0")
            }
        }

        // internal（private ではない）: androidTest/test 側からマイグレーション単体テストで直接使うため。
        internal val MIGRATION_8_9 = object : Migration(8, 9) {
            override fun migrate(db: SupportSQLiteDatabase) {
                // スニペット（定型コマンド）機能: 接続後自動実行コマンド列 + snippets テーブル。
                db.execSQL("ALTER TABLE connection_profiles ADD COLUMN post_connect_commands TEXT")
                db.execSQL("""
                    CREATE TABLE IF NOT EXISTS snippets (
                        id INTEGER PRIMARY KEY AUTOINCREMENT NOT NULL,
                        label TEXT NOT NULL,
                        command TEXT NOT NULL,
                        sort_order INTEGER NOT NULL DEFAULT 0,
                        profile_id INTEGER,
                        append_newline INTEGER NOT NULL DEFAULT 1
                    )
                """.trimIndent())
            }
        }

        // internal（private ではない）: androidTest/test 側からマイグレーション単体テストで直接使うため。
        // ポートフォワード(-L, MVP)を profile ごとに保存するための列を追加する。
        // 既存行は JSON の空配列 "[]"(= フォワードなし)で埋める。
        internal val MIGRATION_9_10 = object : Migration(9, 10) {
            override fun migrate(db: SupportSQLiteDatabase) {
                db.execSQL("ALTER TABLE connection_profiles ADD COLUMN forwards TEXT NOT NULL DEFAULT '[]'")
            }
        }

        fun getInstance(context: Context): AppDatabase =
            instance ?: synchronized(this) {
                instance ?: Room.databaseBuilder(
                    context.applicationContext,
                    AppDatabase::class.java,
                    "tssh.db"
                )
                .addMigrations(
                    MIGRATION_1_2, MIGRATION_2_3, MIGRATION_3_4, MIGRATION_4_5, MIGRATION_5_6, MIGRATION_6_7,
                    MIGRATION_7_8, MIGRATION_8_9, MIGRATION_9_10,
                )
                .build().also { instance = it }
            }
    }
}
