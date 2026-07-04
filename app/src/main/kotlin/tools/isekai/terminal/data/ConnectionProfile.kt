package tools.isekai.terminal.data

import android.os.Parcel
import android.os.Parcelable
import androidx.room.*
import kotlinx.parcelize.Parceler
import kotlinx.parcelize.Parcelize
import kotlinx.parcelize.TypeParceler
import org.json.JSONArray
import org.json.JSONObject
import tools.isekai.terminal.session.PhysicalMultipathFds
import uniffi.tssh_core.ForwardType
import uniffi.tssh_core.HelperQuicConfig
import uniffi.tssh_core.IsekaiLinkRelayConfig
import uniffi.tssh_core.IsekaiStunP2pConfig
import uniffi.tssh_core.JumpConfig
import uniffi.tssh_core.MultipathHelperQuicConfig
import uniffi.tssh_core.PortForward
import uniffi.tssh_core.QuicConfig
import uniffi.tssh_core.SshAuth
import uniffi.tssh_core.SshConfig
import uniffi.tssh_core.TransportPreference

/**
 * [PortForward] は uniffi 生成の素の data class で Parcelable ではないため、
 * `@Parcelize` に読み書き方法を教える(MVP では forwardType は LOCAL 固定なので保存しない)。
 */
private object PortForwardParceler : Parceler<PortForward> {
    override fun create(parcel: Parcel): PortForward = PortForward(
        forwardType = ForwardType.LOCAL,
        bindAddress = parcel.readString() ?: "127.0.0.1",
        bindPort = parcel.readInt().toUShort(),
        remoteHost = parcel.readString() ?: "",
        remotePort = parcel.readInt().toUShort(),
    )

    override fun PortForward.write(parcel: Parcel, flags: Int) {
        parcel.writeString(bindAddress)
        parcel.writeInt(bindPort.toInt())
        parcel.writeString(remoteHost)
        parcel.writeInt(remotePort.toInt())
    }
}

@TypeParceler<PortForward, PortForwardParceler>()
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
    // SSH agent forwarding。既定 OFF・プロファイル単位 opt-in（DESIGN.md では当初対象外
    // だったが方針転換して追加）。有効にすると接続先サーバーが、転送された鍵での
    // 署名をこのアプリに要求できるようになる（署名要求ごとにユーザー確認が必須）。
    @ColumnInfo(name = "enable_agent_forward") val enableAgentForward: Boolean = false,
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
    // 接続確立後に自動実行するコマンド列（改行区切り、複数可）。null/空なら何もしない。
    @ColumnInfo(name = "post_connect_commands") val postConnectCommands: String? = null,
    /** ローカルポートフォワード(-L)一覧。Room には TEXT(JSON) 列として保存する。 */
    @ColumnInfo(name = "forwards", defaultValue = "[]") val forwards: List<PortForward> = emptyList(),
    // 多段SSH(ProxyJump、`ssh -J` 相当)。対象ホストへ直接到達できない場合に、
    // まずこの踏み台ホストへ接続・認証してからトンネルする。null なら直接接続。
    // 踏み台自体の認証情報は主接続と同じ authType/keyId のパターンを踏襲する
    // （password の場合は接続時に別途プロンプトする。ConnectionProfile には平文
    // パスワードを保存しない、という既存方針を踏み台にもそのまま適用する）。
    @ColumnInfo(name = "jump_host") val jumpHost: String? = null,
    @ColumnInfo(name = "jump_port") val jumpPort: Int = 22,
    @ColumnInfo(name = "jump_username") val jumpUsername: String? = null,
    @ColumnInfo(name = "jump_auth_type") val jumpAuthType: String? = null, // "password" | "key"
    @ColumnInfo(name = "jump_key_id") val jumpKeyId: Long? = null,
    // Phase 10: STUN+SSHランデブーによる直接P2P QUIC(TransportPreference.ISEKAI_STUN_P2P_QUIC)
    // 選択時のみ使うSTUNサーバー(host:port)。null/空なら DEFAULT_STUN_SERVER を使う。
    @ColumnInfo(name = "stun_server") val stunServer: String? = null,
    // Phase 10: MASQUE relay経由P2P QUIC(TransportPreference.ISEKAI_LINK_RELAY_QUIC)選択時のみ使う。
    // relayJwtは平文ではなく RelayCredentialVault(KeystoreKek由来のAES/GCM、issue #1対応)で
    // 暗号化した値(Base64)をここに保存する。読み書きは必ず ProfileEditScreen の
    // encryptRelayJwt/decryptRelayJwt(既定は RelayCredentialVault) と
    // AppExecutor.decryptRelayJwt(接続直前)を経由すること。この列に直接平文JWTを
    // 書き込まないこと。access_jwt短命化・refresh/device token化(relay認可サーバー
    // 前提の本格的なcredential vault)はPLAN.md Phase 12以降の設計候補として別途検討。
    @ColumnInfo(name = "relay_addr") val relayAddr: String? = null,
    @ColumnInfo(name = "relay_sni") val relaySni: String? = null,
    @ColumnInfo(name = "relay_jwt") val relayJwt: String? = null,
    // 外部レビュー指摘対応(Phase 11 P0-4): ポートフォワードの非ループバックbindを
    // Rust側(SshConfig.allowNonLoopbackForwardBind)でも明示許可制にするフラグ。
    // 既定false。Kotlin UI側の警告表示だけに頼らず、コア側でも同じ判断を強制する
    // (Rust SSOTルール)。
    @ColumnInfo(name = "allow_non_loopback_forward_bind") val allowNonLoopbackForwardBind: Boolean = false,
    // Phase 12 P2-1: per-session/per-hostのterminal theme。プロファイル単位の配色既定
    // (TerminalThemes のプリセット名)。null ならアプリ全体のグローバル既定(SharedPreferences
    // "tssh_ui")に従う。タブを開いた時点でのみ解決され、タブ内で個別に上書きもできる
    // (Global default → Profile default → Tab/session override、TerminalTabsViewModel参照)。
    @ColumnInfo(name = "theme_name") val themeName: String? = null,
) : Parcelable {
    val transportPreference: TransportPreference
        get() = try {
            TransportPreference.valueOf(transportPreferenceName)
        } catch (_: IllegalArgumentException) {
            TransportPreference.PLAIN_SSH
        }

    /** 踏み台ホストが設定されているか(多段SSHを使うプロファイルか)。 */
    val usesJumpHost: Boolean
        get() = !jumpHost.isNullOrBlank()

    /** relay版P2P QUIC接続に必要な設定が(relayAddr/relaySni/relayJwtの3つとも)揃っているか。 */
    val hasRelayConfig: Boolean
        get() = !relayAddr.isNullOrBlank() && !relaySni.isNullOrBlank() && !relayJwt.isNullOrBlank()

    companion object {
        /** tsshd のデフォルト待受ポート。変更する場合も過去の Room migration 内の
         *  リテラル値（そのマイグレーションを書いた時点のデフォルト、という歴史的記録）は書き換えないこと。 */
        const val DEFAULT_TSSHD_PORT = 2222

        /** [stunServer] 未設定時に使う既定の公開STUNサーバー。双方が同じサーバーを
         *  使う必要は無い(isekai_stun_p2p_transport.rs参照)ため、これは単なるデフォルト値。 */
        const val DEFAULT_STUN_SERVER = "stun.l.google.com:19302"
    }
}

/**
 * [PortForward] のリストを Room の TEXT 列に保存するための TypeConverter。
 * 外部の JSON ライブラリを追加せず、Android 標準の `org.json` だけで完結させている
 * (MVP のポートフォワード機能のためだけに kotlinx.serialization 等を新規導入しない判断)。
 */
object PortForwardListConverter {
    @TypeConverter
    @JvmStatic
    fun fromForwards(forwards: List<PortForward>): String {
        val arr = JSONArray()
        for (f in forwards) {
            val o = JSONObject()
            o.put("type", "LOCAL")
            o.put("bindAddress", f.bindAddress)
            o.put("bindPort", f.bindPort.toInt())
            o.put("remoteHost", f.remoteHost)
            o.put("remotePort", f.remotePort.toInt())
            arr.put(o)
        }
        return arr.toString()
    }

    @TypeConverter
    @JvmStatic
    fun toForwards(json: String): List<PortForward> {
        if (json.isBlank()) return emptyList()
        val arr = JSONArray(json)
        return (0 until arr.length()).map { i ->
            val o = arr.getJSONObject(i)
            PortForward(
                forwardType = ForwardType.LOCAL,
                bindAddress = o.getString("bindAddress"),
                bindPort = o.getInt("bindPort").toUShort(),
                remoteHost = o.getString("remoteHost"),
                remotePort = o.getInt("remotePort").toUShort(),
            )
        }
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

/**
 * [jumpAuth] は [ConnectionProfile.usesJumpHost] が true の場合にのみ使う（呼び出し側が
 * [ConnectionProfile.jumpAuthType]/[ConnectionProfile.jumpKeyId] を解決して渡す。
 * password の場合は接続時プロンプトで得た値、key の場合はキーストアから読んだ PEM）。
 * ブートストラップSSH接続を伴う全トランスポート([toSshConfig]/[toHelperQuicConfig]/
 * [toMultipathHelperQuicConfig])で共通のため、ここに切り出してある。
 */
private fun ConnectionProfile.toJumpConfigOrNull(jumpAuth: SshAuth?): JumpConfig? =
    if (usesJumpHost && jumpAuth != null) {
        JumpConfig(
            host = jumpHost!!,
            port = jumpPort.toUShort(),
            username = jumpUsername ?: "",
            auth = jumpAuth,
        )
    } else {
        null
    }

fun ConnectionProfile.toSshConfig(
    auth: SshAuth,
    jumpAuth: SshAuth? = null,
    cols: UInt = 80u,
    rows: UInt = 24u,
): SshConfig =
    SshConfig(
        host = host,
        port = port.toUShort(),
        username = username,
        auth = auth,
        cols = cols,
        rows = rows,
        forwards = forwards,
        agentForward = enableAgentForward,
        jump = toJumpConfigOrNull(jumpAuth),
        allowNonLoopbackForwardBind = allowNonLoopbackForwardBind,
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

fun ConnectionProfile.toHelperQuicConfig(
    auth: SshAuth,
    jumpAuth: SshAuth? = null,
    cols: UInt = 80u,
    rows: UInt = 24u,
): HelperQuicConfig =
    HelperQuicConfig(
        sshHost = host,
        sshPort = port.toUShort(),
        username = username,
        auth = auth,
        cols = cols,
        rows = rows,
        jump = toJumpConfigOrNull(jumpAuth),
    )

fun ConnectionProfile.toIsekaiStunP2pConfig(
    auth: SshAuth,
    jumpAuth: SshAuth? = null,
    cols: UInt = 80u,
    rows: UInt = 24u,
): IsekaiStunP2pConfig =
    IsekaiStunP2pConfig(
        sshHost = host,
        sshPort = port.toUShort(),
        username = username,
        auth = auth,
        cols = cols,
        rows = rows,
        jump = toJumpConfigOrNull(jumpAuth),
        stunServer = stunServer?.takeIf { it.isNotBlank() } ?: ConnectionProfile.DEFAULT_STUN_SERVER,
    )

/**
 * [relayAddr]/[relaySni]/[relayJwt] は全て必須(呼び出し前に [ConnectionProfile.hasRelayConfig] で
 * 確認すること)。MASQUE relay(`bound-udp-server`)経由のP2P QUIC用。
 */
fun ConnectionProfile.toIsekaiLinkRelayConfig(
    auth: SshAuth,
    jumpAuth: SshAuth? = null,
    cols: UInt = 80u,
    rows: UInt = 24u,
): IsekaiLinkRelayConfig =
    IsekaiLinkRelayConfig(
        sshHost = host,
        sshPort = port.toUShort(),
        username = username,
        auth = auth,
        cols = cols,
        rows = rows,
        jump = toJumpConfigOrNull(jumpAuth),
        relayAddr = relayAddr.orEmpty(),
        relaySni = relaySni.orEmpty(),
        relayJwt = relayJwt.orEmpty(),
    )

fun ConnectionProfile.toMultipathHelperQuicConfig(
    auth: SshAuth,
    physicalFds: PhysicalMultipathFds = PhysicalMultipathFds(),
    jumpAuth: SshAuth? = null,
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
        jump = toJumpConfigOrNull(jumpAuth),
    )
