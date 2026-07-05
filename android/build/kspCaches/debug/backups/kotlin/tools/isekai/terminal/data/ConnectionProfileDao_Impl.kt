package tools.isekai.terminal.`data`

import androidx.room.EntityDeleteOrUpdateAdapter
import androidx.room.EntityInsertAdapter
import androidx.room.RoomDatabase
import androidx.room.util.getColumnIndexOrThrow
import androidx.room.util.performSuspending
import androidx.sqlite.SQLiteStatement
import javax.`annotation`.processing.Generated
import kotlin.Boolean
import kotlin.Int
import kotlin.Long
import kotlin.String
import kotlin.Suppress
import kotlin.Unit
import kotlin.collections.List
import kotlin.collections.MutableList
import kotlin.collections.mutableListOf
import kotlin.reflect.KClass
import uniffi.isekai_terminal_core.PortForward

@Generated(value = ["androidx.room.RoomProcessor"])
@Suppress(names = ["UNCHECKED_CAST", "DEPRECATION", "REDUNDANT_PROJECTION", "REMOVAL"])
public class ConnectionProfileDao_Impl(
  __db: RoomDatabase,
) : ConnectionProfileDao {
  private val __db: RoomDatabase

  private val __insertAdapterOfConnectionProfile: EntityInsertAdapter<ConnectionProfile>

  private val __deleteAdapterOfConnectionProfile: EntityDeleteOrUpdateAdapter<ConnectionProfile>
  init {
    this.__db = __db
    this.__insertAdapterOfConnectionProfile = object : EntityInsertAdapter<ConnectionProfile>() {
      protected override fun createQuery(): String =
          "INSERT OR REPLACE INTO `connection_profiles` (`id`,`label`,`host`,`port`,`username`,`authType`,`keyId`,`sort_order`,`use_tsshd`,`tsshd_port`,`enable_agent_forward`,`transport_preference`,`direct_address`,`enable_physical_multipath`,`cellular_remote_address`,`enable_upstream_failover`,`post_connect_commands`,`forwards`,`jump_host`,`jump_port`,`jump_username`,`jump_auth_type`,`jump_key_id`,`stun_server`,`relay_addr`,`relay_sni`,`relay_jwt`,`allow_non_loopback_forward_bind`,`theme_name`,`helper_bind_port`) VALUES (nullif(?, 0),?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?)"

      protected override fun bind(statement: SQLiteStatement, entity: ConnectionProfile) {
        statement.bindLong(1, entity.id)
        statement.bindText(2, entity.label)
        statement.bindText(3, entity.host)
        statement.bindLong(4, entity.port.toLong())
        statement.bindText(5, entity.username)
        statement.bindText(6, entity.authType)
        val _tmpKeyId: Long? = entity.keyId
        if (_tmpKeyId == null) {
          statement.bindNull(7)
        } else {
          statement.bindLong(7, _tmpKeyId)
        }
        statement.bindLong(8, entity.sortOrder.toLong())
        val _tmp: Int = if (entity.useTsshd) 1 else 0
        statement.bindLong(9, _tmp.toLong())
        statement.bindLong(10, entity.tsshdPort.toLong())
        val _tmp_1: Int = if (entity.enableAgentForward) 1 else 0
        statement.bindLong(11, _tmp_1.toLong())
        statement.bindText(12, entity.transportPreferenceName)
        val _tmpDirectAddress: String? = entity.directAddress
        if (_tmpDirectAddress == null) {
          statement.bindNull(13)
        } else {
          statement.bindText(13, _tmpDirectAddress)
        }
        val _tmp_2: Int = if (entity.enablePhysicalMultipath) 1 else 0
        statement.bindLong(14, _tmp_2.toLong())
        val _tmpCellularRemoteAddress: String? = entity.cellularRemoteAddress
        if (_tmpCellularRemoteAddress == null) {
          statement.bindNull(15)
        } else {
          statement.bindText(15, _tmpCellularRemoteAddress)
        }
        val _tmp_3: Int = if (entity.enableUpstreamFailover) 1 else 0
        statement.bindLong(16, _tmp_3.toLong())
        val _tmpPostConnectCommands: String? = entity.postConnectCommands
        if (_tmpPostConnectCommands == null) {
          statement.bindNull(17)
        } else {
          statement.bindText(17, _tmpPostConnectCommands)
        }
        val _tmp_4: String = PortForwardListConverter.fromForwards(entity.forwards)
        statement.bindText(18, _tmp_4)
        val _tmpJumpHost: String? = entity.jumpHost
        if (_tmpJumpHost == null) {
          statement.bindNull(19)
        } else {
          statement.bindText(19, _tmpJumpHost)
        }
        statement.bindLong(20, entity.jumpPort.toLong())
        val _tmpJumpUsername: String? = entity.jumpUsername
        if (_tmpJumpUsername == null) {
          statement.bindNull(21)
        } else {
          statement.bindText(21, _tmpJumpUsername)
        }
        val _tmpJumpAuthType: String? = entity.jumpAuthType
        if (_tmpJumpAuthType == null) {
          statement.bindNull(22)
        } else {
          statement.bindText(22, _tmpJumpAuthType)
        }
        val _tmpJumpKeyId: Long? = entity.jumpKeyId
        if (_tmpJumpKeyId == null) {
          statement.bindNull(23)
        } else {
          statement.bindLong(23, _tmpJumpKeyId)
        }
        val _tmpStunServer: String? = entity.stunServer
        if (_tmpStunServer == null) {
          statement.bindNull(24)
        } else {
          statement.bindText(24, _tmpStunServer)
        }
        val _tmpRelayAddr: String? = entity.relayAddr
        if (_tmpRelayAddr == null) {
          statement.bindNull(25)
        } else {
          statement.bindText(25, _tmpRelayAddr)
        }
        val _tmpRelaySni: String? = entity.relaySni
        if (_tmpRelaySni == null) {
          statement.bindNull(26)
        } else {
          statement.bindText(26, _tmpRelaySni)
        }
        val _tmpRelayJwt: String? = entity.relayJwt
        if (_tmpRelayJwt == null) {
          statement.bindNull(27)
        } else {
          statement.bindText(27, _tmpRelayJwt)
        }
        val _tmp_5: Int = if (entity.allowNonLoopbackForwardBind) 1 else 0
        statement.bindLong(28, _tmp_5.toLong())
        val _tmpThemeName: String? = entity.themeName
        if (_tmpThemeName == null) {
          statement.bindNull(29)
        } else {
          statement.bindText(29, _tmpThemeName)
        }
        val _tmpHelperBindPort: Int? = entity.helperBindPort
        if (_tmpHelperBindPort == null) {
          statement.bindNull(30)
        } else {
          statement.bindLong(30, _tmpHelperBindPort.toLong())
        }
      }
    }
    this.__deleteAdapterOfConnectionProfile = object :
        EntityDeleteOrUpdateAdapter<ConnectionProfile>() {
      protected override fun createQuery(): String =
          "DELETE FROM `connection_profiles` WHERE `id` = ?"

      protected override fun bind(statement: SQLiteStatement, entity: ConnectionProfile) {
        statement.bindLong(1, entity.id)
      }
    }
  }

  public override suspend fun upsert(profile: ConnectionProfile): Long = performSuspending(__db,
      false, true) { _connection ->
    val _result: Long = __insertAdapterOfConnectionProfile.insertAndReturnId(_connection, profile)
    _result
  }

  public override suspend fun delete(profile: ConnectionProfile): Unit = performSuspending(__db,
      false, true) { _connection ->
    __deleteAdapterOfConnectionProfile.handle(_connection, profile)
  }

  public override suspend fun getAll(): List<ConnectionProfile> {
    val _sql: String = "SELECT * FROM connection_profiles ORDER BY sort_order ASC, label ASC"
    return performSuspending(__db, true, false) { _connection ->
      val _stmt: SQLiteStatement = _connection.prepare(_sql)
      try {
        val _columnIndexOfId: Int = getColumnIndexOrThrow(_stmt, "id")
        val _columnIndexOfLabel: Int = getColumnIndexOrThrow(_stmt, "label")
        val _columnIndexOfHost: Int = getColumnIndexOrThrow(_stmt, "host")
        val _columnIndexOfPort: Int = getColumnIndexOrThrow(_stmt, "port")
        val _columnIndexOfUsername: Int = getColumnIndexOrThrow(_stmt, "username")
        val _columnIndexOfAuthType: Int = getColumnIndexOrThrow(_stmt, "authType")
        val _columnIndexOfKeyId: Int = getColumnIndexOrThrow(_stmt, "keyId")
        val _columnIndexOfSortOrder: Int = getColumnIndexOrThrow(_stmt, "sort_order")
        val _columnIndexOfUseTsshd: Int = getColumnIndexOrThrow(_stmt, "use_tsshd")
        val _columnIndexOfTsshdPort: Int = getColumnIndexOrThrow(_stmt, "tsshd_port")
        val _columnIndexOfEnableAgentForward: Int = getColumnIndexOrThrow(_stmt,
            "enable_agent_forward")
        val _columnIndexOfTransportPreferenceName: Int = getColumnIndexOrThrow(_stmt,
            "transport_preference")
        val _columnIndexOfDirectAddress: Int = getColumnIndexOrThrow(_stmt, "direct_address")
        val _columnIndexOfEnablePhysicalMultipath: Int = getColumnIndexOrThrow(_stmt,
            "enable_physical_multipath")
        val _columnIndexOfCellularRemoteAddress: Int = getColumnIndexOrThrow(_stmt,
            "cellular_remote_address")
        val _columnIndexOfEnableUpstreamFailover: Int = getColumnIndexOrThrow(_stmt,
            "enable_upstream_failover")
        val _columnIndexOfPostConnectCommands: Int = getColumnIndexOrThrow(_stmt,
            "post_connect_commands")
        val _columnIndexOfForwards: Int = getColumnIndexOrThrow(_stmt, "forwards")
        val _columnIndexOfJumpHost: Int = getColumnIndexOrThrow(_stmt, "jump_host")
        val _columnIndexOfJumpPort: Int = getColumnIndexOrThrow(_stmt, "jump_port")
        val _columnIndexOfJumpUsername: Int = getColumnIndexOrThrow(_stmt, "jump_username")
        val _columnIndexOfJumpAuthType: Int = getColumnIndexOrThrow(_stmt, "jump_auth_type")
        val _columnIndexOfJumpKeyId: Int = getColumnIndexOrThrow(_stmt, "jump_key_id")
        val _columnIndexOfStunServer: Int = getColumnIndexOrThrow(_stmt, "stun_server")
        val _columnIndexOfRelayAddr: Int = getColumnIndexOrThrow(_stmt, "relay_addr")
        val _columnIndexOfRelaySni: Int = getColumnIndexOrThrow(_stmt, "relay_sni")
        val _columnIndexOfRelayJwt: Int = getColumnIndexOrThrow(_stmt, "relay_jwt")
        val _columnIndexOfAllowNonLoopbackForwardBind: Int = getColumnIndexOrThrow(_stmt,
            "allow_non_loopback_forward_bind")
        val _columnIndexOfThemeName: Int = getColumnIndexOrThrow(_stmt, "theme_name")
        val _columnIndexOfHelperBindPort: Int = getColumnIndexOrThrow(_stmt, "helper_bind_port")
        val _result: MutableList<ConnectionProfile> = mutableListOf()
        while (_stmt.step()) {
          val _item: ConnectionProfile
          val _tmpId: Long
          _tmpId = _stmt.getLong(_columnIndexOfId)
          val _tmpLabel: String
          _tmpLabel = _stmt.getText(_columnIndexOfLabel)
          val _tmpHost: String
          _tmpHost = _stmt.getText(_columnIndexOfHost)
          val _tmpPort: Int
          _tmpPort = _stmt.getLong(_columnIndexOfPort).toInt()
          val _tmpUsername: String
          _tmpUsername = _stmt.getText(_columnIndexOfUsername)
          val _tmpAuthType: String
          _tmpAuthType = _stmt.getText(_columnIndexOfAuthType)
          val _tmpKeyId: Long?
          if (_stmt.isNull(_columnIndexOfKeyId)) {
            _tmpKeyId = null
          } else {
            _tmpKeyId = _stmt.getLong(_columnIndexOfKeyId)
          }
          val _tmpSortOrder: Int
          _tmpSortOrder = _stmt.getLong(_columnIndexOfSortOrder).toInt()
          val _tmpUseTsshd: Boolean
          val _tmp: Int
          _tmp = _stmt.getLong(_columnIndexOfUseTsshd).toInt()
          _tmpUseTsshd = _tmp != 0
          val _tmpTsshdPort: Int
          _tmpTsshdPort = _stmt.getLong(_columnIndexOfTsshdPort).toInt()
          val _tmpEnableAgentForward: Boolean
          val _tmp_1: Int
          _tmp_1 = _stmt.getLong(_columnIndexOfEnableAgentForward).toInt()
          _tmpEnableAgentForward = _tmp_1 != 0
          val _tmpTransportPreferenceName: String
          _tmpTransportPreferenceName = _stmt.getText(_columnIndexOfTransportPreferenceName)
          val _tmpDirectAddress: String?
          if (_stmt.isNull(_columnIndexOfDirectAddress)) {
            _tmpDirectAddress = null
          } else {
            _tmpDirectAddress = _stmt.getText(_columnIndexOfDirectAddress)
          }
          val _tmpEnablePhysicalMultipath: Boolean
          val _tmp_2: Int
          _tmp_2 = _stmt.getLong(_columnIndexOfEnablePhysicalMultipath).toInt()
          _tmpEnablePhysicalMultipath = _tmp_2 != 0
          val _tmpCellularRemoteAddress: String?
          if (_stmt.isNull(_columnIndexOfCellularRemoteAddress)) {
            _tmpCellularRemoteAddress = null
          } else {
            _tmpCellularRemoteAddress = _stmt.getText(_columnIndexOfCellularRemoteAddress)
          }
          val _tmpEnableUpstreamFailover: Boolean
          val _tmp_3: Int
          _tmp_3 = _stmt.getLong(_columnIndexOfEnableUpstreamFailover).toInt()
          _tmpEnableUpstreamFailover = _tmp_3 != 0
          val _tmpPostConnectCommands: String?
          if (_stmt.isNull(_columnIndexOfPostConnectCommands)) {
            _tmpPostConnectCommands = null
          } else {
            _tmpPostConnectCommands = _stmt.getText(_columnIndexOfPostConnectCommands)
          }
          val _tmpForwards: List<PortForward>
          val _tmp_4: String
          _tmp_4 = _stmt.getText(_columnIndexOfForwards)
          _tmpForwards = PortForwardListConverter.toForwards(_tmp_4)
          val _tmpJumpHost: String?
          if (_stmt.isNull(_columnIndexOfJumpHost)) {
            _tmpJumpHost = null
          } else {
            _tmpJumpHost = _stmt.getText(_columnIndexOfJumpHost)
          }
          val _tmpJumpPort: Int
          _tmpJumpPort = _stmt.getLong(_columnIndexOfJumpPort).toInt()
          val _tmpJumpUsername: String?
          if (_stmt.isNull(_columnIndexOfJumpUsername)) {
            _tmpJumpUsername = null
          } else {
            _tmpJumpUsername = _stmt.getText(_columnIndexOfJumpUsername)
          }
          val _tmpJumpAuthType: String?
          if (_stmt.isNull(_columnIndexOfJumpAuthType)) {
            _tmpJumpAuthType = null
          } else {
            _tmpJumpAuthType = _stmt.getText(_columnIndexOfJumpAuthType)
          }
          val _tmpJumpKeyId: Long?
          if (_stmt.isNull(_columnIndexOfJumpKeyId)) {
            _tmpJumpKeyId = null
          } else {
            _tmpJumpKeyId = _stmt.getLong(_columnIndexOfJumpKeyId)
          }
          val _tmpStunServer: String?
          if (_stmt.isNull(_columnIndexOfStunServer)) {
            _tmpStunServer = null
          } else {
            _tmpStunServer = _stmt.getText(_columnIndexOfStunServer)
          }
          val _tmpRelayAddr: String?
          if (_stmt.isNull(_columnIndexOfRelayAddr)) {
            _tmpRelayAddr = null
          } else {
            _tmpRelayAddr = _stmt.getText(_columnIndexOfRelayAddr)
          }
          val _tmpRelaySni: String?
          if (_stmt.isNull(_columnIndexOfRelaySni)) {
            _tmpRelaySni = null
          } else {
            _tmpRelaySni = _stmt.getText(_columnIndexOfRelaySni)
          }
          val _tmpRelayJwt: String?
          if (_stmt.isNull(_columnIndexOfRelayJwt)) {
            _tmpRelayJwt = null
          } else {
            _tmpRelayJwt = _stmt.getText(_columnIndexOfRelayJwt)
          }
          val _tmpAllowNonLoopbackForwardBind: Boolean
          val _tmp_5: Int
          _tmp_5 = _stmt.getLong(_columnIndexOfAllowNonLoopbackForwardBind).toInt()
          _tmpAllowNonLoopbackForwardBind = _tmp_5 != 0
          val _tmpThemeName: String?
          if (_stmt.isNull(_columnIndexOfThemeName)) {
            _tmpThemeName = null
          } else {
            _tmpThemeName = _stmt.getText(_columnIndexOfThemeName)
          }
          val _tmpHelperBindPort: Int?
          if (_stmt.isNull(_columnIndexOfHelperBindPort)) {
            _tmpHelperBindPort = null
          } else {
            _tmpHelperBindPort = _stmt.getLong(_columnIndexOfHelperBindPort).toInt()
          }
          _item =
              ConnectionProfile(_tmpId,_tmpLabel,_tmpHost,_tmpPort,_tmpUsername,_tmpAuthType,_tmpKeyId,_tmpSortOrder,_tmpUseTsshd,_tmpTsshdPort,_tmpEnableAgentForward,_tmpTransportPreferenceName,_tmpDirectAddress,_tmpEnablePhysicalMultipath,_tmpCellularRemoteAddress,_tmpEnableUpstreamFailover,_tmpPostConnectCommands,_tmpForwards,_tmpJumpHost,_tmpJumpPort,_tmpJumpUsername,_tmpJumpAuthType,_tmpJumpKeyId,_tmpStunServer,_tmpRelayAddr,_tmpRelaySni,_tmpRelayJwt,_tmpAllowNonLoopbackForwardBind,_tmpThemeName,_tmpHelperBindPort)
          _result.add(_item)
        }
        _result
      } finally {
        _stmt.close()
      }
    }
  }

  public override suspend fun findById(id: Long): ConnectionProfile? {
    val _sql: String = "SELECT * FROM connection_profiles WHERE id = ? LIMIT 1"
    return performSuspending(__db, true, false) { _connection ->
      val _stmt: SQLiteStatement = _connection.prepare(_sql)
      try {
        var _argIndex: Int = 1
        _stmt.bindLong(_argIndex, id)
        val _columnIndexOfId: Int = getColumnIndexOrThrow(_stmt, "id")
        val _columnIndexOfLabel: Int = getColumnIndexOrThrow(_stmt, "label")
        val _columnIndexOfHost: Int = getColumnIndexOrThrow(_stmt, "host")
        val _columnIndexOfPort: Int = getColumnIndexOrThrow(_stmt, "port")
        val _columnIndexOfUsername: Int = getColumnIndexOrThrow(_stmt, "username")
        val _columnIndexOfAuthType: Int = getColumnIndexOrThrow(_stmt, "authType")
        val _columnIndexOfKeyId: Int = getColumnIndexOrThrow(_stmt, "keyId")
        val _columnIndexOfSortOrder: Int = getColumnIndexOrThrow(_stmt, "sort_order")
        val _columnIndexOfUseTsshd: Int = getColumnIndexOrThrow(_stmt, "use_tsshd")
        val _columnIndexOfTsshdPort: Int = getColumnIndexOrThrow(_stmt, "tsshd_port")
        val _columnIndexOfEnableAgentForward: Int = getColumnIndexOrThrow(_stmt,
            "enable_agent_forward")
        val _columnIndexOfTransportPreferenceName: Int = getColumnIndexOrThrow(_stmt,
            "transport_preference")
        val _columnIndexOfDirectAddress: Int = getColumnIndexOrThrow(_stmt, "direct_address")
        val _columnIndexOfEnablePhysicalMultipath: Int = getColumnIndexOrThrow(_stmt,
            "enable_physical_multipath")
        val _columnIndexOfCellularRemoteAddress: Int = getColumnIndexOrThrow(_stmt,
            "cellular_remote_address")
        val _columnIndexOfEnableUpstreamFailover: Int = getColumnIndexOrThrow(_stmt,
            "enable_upstream_failover")
        val _columnIndexOfPostConnectCommands: Int = getColumnIndexOrThrow(_stmt,
            "post_connect_commands")
        val _columnIndexOfForwards: Int = getColumnIndexOrThrow(_stmt, "forwards")
        val _columnIndexOfJumpHost: Int = getColumnIndexOrThrow(_stmt, "jump_host")
        val _columnIndexOfJumpPort: Int = getColumnIndexOrThrow(_stmt, "jump_port")
        val _columnIndexOfJumpUsername: Int = getColumnIndexOrThrow(_stmt, "jump_username")
        val _columnIndexOfJumpAuthType: Int = getColumnIndexOrThrow(_stmt, "jump_auth_type")
        val _columnIndexOfJumpKeyId: Int = getColumnIndexOrThrow(_stmt, "jump_key_id")
        val _columnIndexOfStunServer: Int = getColumnIndexOrThrow(_stmt, "stun_server")
        val _columnIndexOfRelayAddr: Int = getColumnIndexOrThrow(_stmt, "relay_addr")
        val _columnIndexOfRelaySni: Int = getColumnIndexOrThrow(_stmt, "relay_sni")
        val _columnIndexOfRelayJwt: Int = getColumnIndexOrThrow(_stmt, "relay_jwt")
        val _columnIndexOfAllowNonLoopbackForwardBind: Int = getColumnIndexOrThrow(_stmt,
            "allow_non_loopback_forward_bind")
        val _columnIndexOfThemeName: Int = getColumnIndexOrThrow(_stmt, "theme_name")
        val _columnIndexOfHelperBindPort: Int = getColumnIndexOrThrow(_stmt, "helper_bind_port")
        val _result: ConnectionProfile?
        if (_stmt.step()) {
          val _tmpId: Long
          _tmpId = _stmt.getLong(_columnIndexOfId)
          val _tmpLabel: String
          _tmpLabel = _stmt.getText(_columnIndexOfLabel)
          val _tmpHost: String
          _tmpHost = _stmt.getText(_columnIndexOfHost)
          val _tmpPort: Int
          _tmpPort = _stmt.getLong(_columnIndexOfPort).toInt()
          val _tmpUsername: String
          _tmpUsername = _stmt.getText(_columnIndexOfUsername)
          val _tmpAuthType: String
          _tmpAuthType = _stmt.getText(_columnIndexOfAuthType)
          val _tmpKeyId: Long?
          if (_stmt.isNull(_columnIndexOfKeyId)) {
            _tmpKeyId = null
          } else {
            _tmpKeyId = _stmt.getLong(_columnIndexOfKeyId)
          }
          val _tmpSortOrder: Int
          _tmpSortOrder = _stmt.getLong(_columnIndexOfSortOrder).toInt()
          val _tmpUseTsshd: Boolean
          val _tmp: Int
          _tmp = _stmt.getLong(_columnIndexOfUseTsshd).toInt()
          _tmpUseTsshd = _tmp != 0
          val _tmpTsshdPort: Int
          _tmpTsshdPort = _stmt.getLong(_columnIndexOfTsshdPort).toInt()
          val _tmpEnableAgentForward: Boolean
          val _tmp_1: Int
          _tmp_1 = _stmt.getLong(_columnIndexOfEnableAgentForward).toInt()
          _tmpEnableAgentForward = _tmp_1 != 0
          val _tmpTransportPreferenceName: String
          _tmpTransportPreferenceName = _stmt.getText(_columnIndexOfTransportPreferenceName)
          val _tmpDirectAddress: String?
          if (_stmt.isNull(_columnIndexOfDirectAddress)) {
            _tmpDirectAddress = null
          } else {
            _tmpDirectAddress = _stmt.getText(_columnIndexOfDirectAddress)
          }
          val _tmpEnablePhysicalMultipath: Boolean
          val _tmp_2: Int
          _tmp_2 = _stmt.getLong(_columnIndexOfEnablePhysicalMultipath).toInt()
          _tmpEnablePhysicalMultipath = _tmp_2 != 0
          val _tmpCellularRemoteAddress: String?
          if (_stmt.isNull(_columnIndexOfCellularRemoteAddress)) {
            _tmpCellularRemoteAddress = null
          } else {
            _tmpCellularRemoteAddress = _stmt.getText(_columnIndexOfCellularRemoteAddress)
          }
          val _tmpEnableUpstreamFailover: Boolean
          val _tmp_3: Int
          _tmp_3 = _stmt.getLong(_columnIndexOfEnableUpstreamFailover).toInt()
          _tmpEnableUpstreamFailover = _tmp_3 != 0
          val _tmpPostConnectCommands: String?
          if (_stmt.isNull(_columnIndexOfPostConnectCommands)) {
            _tmpPostConnectCommands = null
          } else {
            _tmpPostConnectCommands = _stmt.getText(_columnIndexOfPostConnectCommands)
          }
          val _tmpForwards: List<PortForward>
          val _tmp_4: String
          _tmp_4 = _stmt.getText(_columnIndexOfForwards)
          _tmpForwards = PortForwardListConverter.toForwards(_tmp_4)
          val _tmpJumpHost: String?
          if (_stmt.isNull(_columnIndexOfJumpHost)) {
            _tmpJumpHost = null
          } else {
            _tmpJumpHost = _stmt.getText(_columnIndexOfJumpHost)
          }
          val _tmpJumpPort: Int
          _tmpJumpPort = _stmt.getLong(_columnIndexOfJumpPort).toInt()
          val _tmpJumpUsername: String?
          if (_stmt.isNull(_columnIndexOfJumpUsername)) {
            _tmpJumpUsername = null
          } else {
            _tmpJumpUsername = _stmt.getText(_columnIndexOfJumpUsername)
          }
          val _tmpJumpAuthType: String?
          if (_stmt.isNull(_columnIndexOfJumpAuthType)) {
            _tmpJumpAuthType = null
          } else {
            _tmpJumpAuthType = _stmt.getText(_columnIndexOfJumpAuthType)
          }
          val _tmpJumpKeyId: Long?
          if (_stmt.isNull(_columnIndexOfJumpKeyId)) {
            _tmpJumpKeyId = null
          } else {
            _tmpJumpKeyId = _stmt.getLong(_columnIndexOfJumpKeyId)
          }
          val _tmpStunServer: String?
          if (_stmt.isNull(_columnIndexOfStunServer)) {
            _tmpStunServer = null
          } else {
            _tmpStunServer = _stmt.getText(_columnIndexOfStunServer)
          }
          val _tmpRelayAddr: String?
          if (_stmt.isNull(_columnIndexOfRelayAddr)) {
            _tmpRelayAddr = null
          } else {
            _tmpRelayAddr = _stmt.getText(_columnIndexOfRelayAddr)
          }
          val _tmpRelaySni: String?
          if (_stmt.isNull(_columnIndexOfRelaySni)) {
            _tmpRelaySni = null
          } else {
            _tmpRelaySni = _stmt.getText(_columnIndexOfRelaySni)
          }
          val _tmpRelayJwt: String?
          if (_stmt.isNull(_columnIndexOfRelayJwt)) {
            _tmpRelayJwt = null
          } else {
            _tmpRelayJwt = _stmt.getText(_columnIndexOfRelayJwt)
          }
          val _tmpAllowNonLoopbackForwardBind: Boolean
          val _tmp_5: Int
          _tmp_5 = _stmt.getLong(_columnIndexOfAllowNonLoopbackForwardBind).toInt()
          _tmpAllowNonLoopbackForwardBind = _tmp_5 != 0
          val _tmpThemeName: String?
          if (_stmt.isNull(_columnIndexOfThemeName)) {
            _tmpThemeName = null
          } else {
            _tmpThemeName = _stmt.getText(_columnIndexOfThemeName)
          }
          val _tmpHelperBindPort: Int?
          if (_stmt.isNull(_columnIndexOfHelperBindPort)) {
            _tmpHelperBindPort = null
          } else {
            _tmpHelperBindPort = _stmt.getLong(_columnIndexOfHelperBindPort).toInt()
          }
          _result =
              ConnectionProfile(_tmpId,_tmpLabel,_tmpHost,_tmpPort,_tmpUsername,_tmpAuthType,_tmpKeyId,_tmpSortOrder,_tmpUseTsshd,_tmpTsshdPort,_tmpEnableAgentForward,_tmpTransportPreferenceName,_tmpDirectAddress,_tmpEnablePhysicalMultipath,_tmpCellularRemoteAddress,_tmpEnableUpstreamFailover,_tmpPostConnectCommands,_tmpForwards,_tmpJumpHost,_tmpJumpPort,_tmpJumpUsername,_tmpJumpAuthType,_tmpJumpKeyId,_tmpStunServer,_tmpRelayAddr,_tmpRelaySni,_tmpRelayJwt,_tmpAllowNonLoopbackForwardBind,_tmpThemeName,_tmpHelperBindPort)
        } else {
          _result = null
        }
        _result
      } finally {
        _stmt.close()
      }
    }
  }

  public companion object {
    public fun getRequiredConverters(): List<KClass<*>> = emptyList()
  }
}
