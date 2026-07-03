package tools.isekai.terminal.data

import android.os.Parcelable
import androidx.room.*
import kotlinx.parcelize.Parcelize
import tools.isekai.terminal.session.PhysicalMultipathFds
import uniffi.tssh_core.HelperQuicConfig
import uniffi.tssh_core.MultipathHelperQuicConfig
import uniffi.tssh_core.QuicConfig
import uniffi.tssh_core.SshAuth
import uniffi.tssh_core.SshConfig
import uniffi.tssh_core.TransportPreference

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
    @ColumnInfo(name = "tsshd_port") val tsshdPort: Int = DEFAULT_TSSHD_PORT,
    // Phase 7: トランスポート戦略。DB には TransportPreference の name() を文字列で保存する。
    @ColumnInfo(name = "transport_preference") val transportPreferenceName: String = TransportPreference.PLAIN_SSH.name,
    // Phase 9: マルチパス(path1)用の直接到達アドレス。IsekaiHelperQuicMultipath 選択時のみ使う。
    @ColumnInfo(name = "direct_address") val directAddress: String? = null,
    // Phase 9-4（実験的機能、既定OFF）: Wi-Fi/セルラー物理無線への同時マルチパスも試す。
    // Tailscale稼働中はbindSocket自体が失敗するため効果が無い（日和見的にpath0/path1へ
    // フォールバックするだけで、明示エラーにはしない）。
    @ColumnInfo(name = "enable_physical_multipath") val enablePhysicalMultipath: Boolean = false,
    // Phase 9-4追加検証（実験的機能）: セルラー物理path候補だけdirectAddressとは別の
    // リモートアドレス（IPv6等）へ向ける。同一remoteに複数local IPでopen_pathすると
    // noq側でvalidationが失敗する実機での発見への回避策。未指定ならdirectAddressと同じ。
    @ColumnInfo(name = "cellular_remote_address") val cellularRemoteAddress: String? = null,
    // 「WiFiは繋がっているがupstreamが死んでいる」（カフェ等のキャプティブポータル）を
    // 検知したら、noqのopen_path()同時オープン（noq issue #738で判明した不具合）を
    // 使わず、Endpoint::rebind_abstract()でセルラーへ丸ごと切り替える。実験的機能・既定OFF。
    @ColumnInfo(name = "enable_upstream_failover") val enableUpstreamFailover: Boolean = false,
) : Parcelable {
    val transportPreference: TransportPreference
        get() = try {
            TransportPreference.valueOf(transportPreferenceName)
        } catch (_: IllegalArgumentException) {
            TransportPreference.PLAIN_SSH
        }

    companion object {
        /** tsshd のデフォルト待受ポート。変更する場合も過去の Room migration 内の
         *  リテラル値（そのマイグレーションを書いた時点のデフォルト、という歴史的記録）は書き換えないこと。 */
        const val DEFAULT_TSSHD_PORT = 2222
    }
}

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

fun ConnectionProfile.toHelperQuicConfig(auth: SshAuth, cols: UInt = 80u, rows: UInt = 24u): HelperQuicConfig =
    HelperQuicConfig(
        sshHost = host,
        sshPort = port.toUShort(),
        username = username,
        auth = auth,
        cols = cols,
        rows = rows,
    )

fun ConnectionProfile.toMultipathHelperQuicConfig(
    auth: SshAuth,
    physicalFds: PhysicalMultipathFds = PhysicalMultipathFds(),
    cols: UInt = 80u,
    rows: UInt = 24u,
): MultipathHelperQuicConfig =
    MultipathHelperQuicConfig(
        sshHost = host,
        sshPort = port.toUShort(),
        directHost = directAddress?.takeIf { it.isNotBlank() },
        cellularRemoteHost = cellularRemoteAddress?.takeIf { it.isNotBlank() },
        wifiFd = physicalFds.wifiFd,
        wifiLocalIp = physicalFds.wifiLocalIp,
        cellularFd = physicalFds.cellularFd,
        cellularLocalIp = physicalFds.cellularLocalIp,
        username = username,
        auth = auth,
        cols = cols,
        rows = rows,
    )
