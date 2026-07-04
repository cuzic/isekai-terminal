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
    version = 17,
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

        // internal（private ではない）: androidTest/test 側からマイグレーション単体テストで直接使うため。
        // SSH agent forwarding。既定 OFF・プロファイル単位 opt-in。
        internal val MIGRATION_10_11 = object : Migration(10, 11) {
            override fun migrate(db: SupportSQLiteDatabase) {
                db.execSQL("ALTER TABLE connection_profiles ADD COLUMN enable_agent_forward INTEGER NOT NULL DEFAULT 0")
            }
        }

        // internal（private ではない）: androidTest/test 側からマイグレーション単体テストで直接使うため。
        // 多段SSH(ProxyJump)。対象ホストへ直接到達できない場合の踏み台ホスト設定。
        // 未設定(jump_host が NULL)なら直接接続のまま挙動は変わらない。
        internal val MIGRATION_11_12 = object : Migration(11, 12) {
            override fun migrate(db: SupportSQLiteDatabase) {
                db.execSQL("ALTER TABLE connection_profiles ADD COLUMN jump_host TEXT")
                db.execSQL("ALTER TABLE connection_profiles ADD COLUMN jump_port INTEGER NOT NULL DEFAULT 22")
                db.execSQL("ALTER TABLE connection_profiles ADD COLUMN jump_username TEXT")
                db.execSQL("ALTER TABLE connection_profiles ADD COLUMN jump_auth_type TEXT")
                db.execSQL("ALTER TABLE connection_profiles ADD COLUMN jump_key_id INTEGER")
            }
        }

        // internal（private ではない）: androidTest/test 側からマイグレーション単体テストで直接使うため。
        // Phase 10: STUN+SSHランデブーによる直接P2P QUIC(TransportPreference.ISEKAI_STUN_P2P_QUIC)用の
        // STUNサーバー設定。未設定(NULL)なら接続時に既定の公開STUNサーバーを使う
        // （`ConnectionProfile.DEFAULT_STUN_SERVER`参照）。
        internal val MIGRATION_12_13 = object : Migration(12, 13) {
            override fun migrate(db: SupportSQLiteDatabase) {
                db.execSQL("ALTER TABLE connection_profiles ADD COLUMN stun_server TEXT")
            }
        }

        // internal（private ではない）: androidTest/test 側からマイグレーション単体テストで直接使うため。
        // Phase 10: MASQUE relay経由P2P QUIC(TransportPreference.ISEKAI_LINK_RELAY_QUIC)用の
        // relayアドレス/SNI/JWT設定。3つとも未設定なら選択できない(ProfileEditScreen参照)。
        internal val MIGRATION_13_14 = object : Migration(13, 14) {
            override fun migrate(db: SupportSQLiteDatabase) {
                db.execSQL("ALTER TABLE connection_profiles ADD COLUMN relay_addr TEXT")
                db.execSQL("ALTER TABLE connection_profiles ADD COLUMN relay_sni TEXT")
                db.execSQL("ALTER TABLE connection_profiles ADD COLUMN relay_jwt TEXT")
            }
        }

        // 外部レビュー指摘対応(Phase 11 P0-4): 非ループバックport forward bindを
        // Rust側(SshConfig.allowNonLoopbackForwardBind)でも明示許可制にするためのフラグ。
        // 既定falseでKotlin UI警告時と同じ「許可しない」挙動を維持する。
        internal val MIGRATION_14_15 = object : Migration(14, 15) {
            override fun migrate(db: SupportSQLiteDatabase) {
                db.execSQL(
                    "ALTER TABLE connection_profiles ADD COLUMN allow_non_loopback_forward_bind " +
                        "INTEGER NOT NULL DEFAULT 0"
                )
            }
        }

        // Phase 12 P2-1: per-session/per-hostのterminal theme。プロファイル単位で
        // 配色テーマの既定を持てるようにする(null ならアプリ全体のグローバル既定に従う)。
        internal val MIGRATION_15_16 = object : Migration(15, 16) {
            override fun migrate(db: SupportSQLiteDatabase) {
                db.execSQL("ALTER TABLE connection_profiles ADD COLUMN theme_name TEXT")
            }
        }

        // 自作ヘルパーQUICの待受ポートをユーザーがプロファイル単位で固定できるようにする
        // (未指定ならこれまで通りOSが選ぶエフェメラルポート)。ファイアウォール越しの
        // direct_address到達性のために単一ポートだけ開ければよいようにするための設定
        // (PLAN.md Phase 7-5/9-2参照)。Rust側への配線は別途対応。
        internal val MIGRATION_16_17 = object : Migration(16, 17) {
            override fun migrate(db: SupportSQLiteDatabase) {
                db.execSQL("ALTER TABLE connection_profiles ADD COLUMN helper_bind_port INTEGER")
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
                    MIGRATION_7_8, MIGRATION_8_9, MIGRATION_9_10, MIGRATION_10_11, MIGRATION_11_12,
                    MIGRATION_12_13, MIGRATION_13_14, MIGRATION_14_15, MIGRATION_15_16, MIGRATION_16_17,
                )
                .build().also { instance = it }
            }
    }
}
