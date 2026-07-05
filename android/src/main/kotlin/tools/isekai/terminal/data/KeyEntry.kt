package tools.isekai.terminal.data

import androidx.room.*

@Entity(tableName = "key_entries")
data class KeyEntry(
    @PrimaryKey(autoGenerate = true) val id: Long = 0,
    val label: String,
    val publicKey: String,
    val encryptedPrivateKeyPath: String,
    val kekAlias: String,
    val createdAt: Long,
)

@Dao
interface KeyEntryDao {
    @Query("SELECT * FROM key_entries ORDER BY label ASC")
    suspend fun getAll(): List<KeyEntry>

    @Insert(onConflict = OnConflictStrategy.REPLACE)
    suspend fun upsert(key: KeyEntry): Long

    @Delete
    suspend fun delete(key: KeyEntry)

    @Query("SELECT * FROM key_entries WHERE id = :id LIMIT 1")
    suspend fun findById(id: Long): KeyEntry?
}
