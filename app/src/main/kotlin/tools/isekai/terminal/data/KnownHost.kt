package tools.isekai.terminal.data

import androidx.room.*

@Entity(tableName = "known_hosts", indices = [Index(value = ["host", "port"], unique = true)])
data class KnownHost(
    @PrimaryKey(autoGenerate = true) val id: Long = 0,
    val host: String,
    val port: Int,
    val keyType: String,
    val fingerprintSha256: String,
    val firstSeenAt: Long,   // epoch millis
    val lastSeenAt: Long,
)

@Dao
interface KnownHostDao {
    @Query("SELECT * FROM known_hosts WHERE host = :host AND port = :port LIMIT 1")
    suspend fun findByHostPort(host: String, port: Int): KnownHost?

    @Insert(onConflict = OnConflictStrategy.REPLACE)
    suspend fun upsert(host: KnownHost)

    @Delete
    suspend fun delete(host: KnownHost)

    @Query("SELECT * FROM known_hosts ORDER BY host ASC")
    suspend fun getAll(): List<KnownHost>
}
