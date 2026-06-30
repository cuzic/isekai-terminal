package com.example.imespike.data

import android.content.Context
import androidx.room.Database
import androidx.room.Room
import androidx.room.RoomDatabase

@Database(
    entities = [KnownHost::class, ConnectionProfile::class, KeyEntry::class],
    version = 1,
    exportSchema = false,
)
abstract class AppDatabase : RoomDatabase() {
    abstract fun knownHostDao(): KnownHostDao
    abstract fun connectionProfileDao(): ConnectionProfileDao
    abstract fun keyEntryDao(): KeyEntryDao

    companion object {
        @Volatile private var instance: AppDatabase? = null

        fun getInstance(context: Context): AppDatabase =
            instance ?: synchronized(this) {
                instance ?: Room.databaseBuilder(
                    context.applicationContext,
                    AppDatabase::class.java,
                    "tssh.db"
                ).build().also { instance = it }
            }
    }
}
