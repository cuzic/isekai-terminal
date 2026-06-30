package com.example.imespike.data

import android.content.Context
import androidx.room.Database
import androidx.room.Room
import androidx.room.RoomDatabase
import androidx.room.migration.Migration
import androidx.sqlite.db.SupportSQLiteDatabase

@Database(
    entities = [KnownHost::class, ConnectionProfile::class, KeyEntry::class],
    version = 2,
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

        fun getInstance(context: Context): AppDatabase =
            instance ?: synchronized(this) {
                instance ?: Room.databaseBuilder(
                    context.applicationContext,
                    AppDatabase::class.java,
                    "tssh.db"
                )
                .addMigrations(MIGRATION_1_2)
                .build().also { instance = it }
            }
    }
}
