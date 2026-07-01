package tools.isekai.terminal.data

import android.os.Parcelable
import androidx.room.*
import kotlinx.parcelize.Parcelize
import uniffi.tssh_core.QuicConfig
import uniffi.tssh_core.SshAuth
import uniffi.tssh_core.SshConfig

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
    @ColumnInfo(name = "sort_order") val sortOrder: Int = 0,
    @ColumnInfo(name = "use_tsshd") val useTsshd: Boolean = false,
    @ColumnInfo(name = "tsshd_port") val tsshdPort: Int = 2222,
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

fun ConnectionProfile.toSshConfig(auth: SshAuth, cols: UInt = 80u, rows: UInt = 24u): SshConfig =
    SshConfig(
        host = host,
        port = port.toUShort(),
        username = username,
        auth = auth,
        cols = cols,
        rows = rows,
    )

fun ConnectionProfile.toQuicConfig(auth: SshAuth, cols: UInt = 80u, rows: UInt = 24u): QuicConfig =
    QuicConfig(
        tsshdHost = host,
        tsshdPort = tsshdPort.toUShort(),
        sshHost = host,
        sshPort = port.toUShort(),
        username = username,
        auth = auth,
        cols = cols,
        rows = rows,
        skipCertVerify = true,
    )
