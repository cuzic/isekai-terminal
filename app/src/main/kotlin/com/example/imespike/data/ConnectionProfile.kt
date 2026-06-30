package com.example.imespike.data

import android.os.Parcelable
import androidx.room.*
import kotlinx.parcelize.Parcelize

@Parcelize
@Entity(tableName = "connection_profiles")
data class ConnectionProfile(
    @PrimaryKey(autoGenerate = true) val id: Long = 0,
    val label: String,
    val host: String,
    val port: Int = 22,
    val username: String,
    val authType: String,    // "password" | "key"
    val keyId: Long? = null,
    val useMosh: Boolean = false,
    @ColumnInfo(name = "sort_order") val sortOrder: Int = 0,
) : Parcelable

@Dao
interface ConnectionProfileDao {
    @Query("SELECT * FROM connection_profiles ORDER BY sort_order ASC, label ASC")
    suspend fun getAll(): List<ConnectionProfile>

    @Insert(onConflict = OnConflictStrategy.REPLACE)
    suspend fun upsert(profile: ConnectionProfile): Long

    @Delete
    suspend fun delete(profile: ConnectionProfile)

    @Query("SELECT * FROM connection_profiles WHERE id = :id LIMIT 1")
    suspend fun findById(id: Long): ConnectionProfile?
}
