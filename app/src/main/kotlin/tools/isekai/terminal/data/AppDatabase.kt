package tools.isekai.terminal.data

import android.content.Context
import androidx.room.Database
import androidx.room.Room
import androidx.room.RoomDatabase
import androidx.room.migration.Migration
import androidx.sqlite.db.SupportSQLiteDatabase

@Database(
    entities = [KnownHost::class, ConnectionProfile::class, KeyEntry::class],
    version = 3,
    exportSchema = false,
)
abstract class AppDatabase : RoomDatabase() {
    abstract fun knownHostDao(): KnownHostDao
    abstract fun connectionProfileDao(): ConnectionProfileDao
    abstract fun keyEntryDao(): KeyEntryDao

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

        fun getInstance(context: Context): AppDatabase =
            instance ?: synchronized(this) {
                instance ?: Room.databaseBuilder(
                    context.applicationContext,
                    AppDatabase::class.java,
                    "tssh.db"
                )
                .addMigrations(MIGRATION_1_2, MIGRATION_2_3)
                .build().also { instance = it }
            }
    }
}
