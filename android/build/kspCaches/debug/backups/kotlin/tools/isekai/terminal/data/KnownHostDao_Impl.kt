package tools.isekai.terminal.`data`

import androidx.room.EntityDeleteOrUpdateAdapter
import androidx.room.EntityInsertAdapter
import androidx.room.RoomDatabase
import androidx.room.util.getColumnIndexOrThrow
import androidx.room.util.performSuspending
import androidx.sqlite.SQLiteStatement
import javax.`annotation`.processing.Generated
import kotlin.Int
import kotlin.Long
import kotlin.String
import kotlin.Suppress
import kotlin.Unit
import kotlin.collections.List
import kotlin.collections.MutableList
import kotlin.collections.mutableListOf
import kotlin.reflect.KClass

@Generated(value = ["androidx.room.RoomProcessor"])
@Suppress(names = ["UNCHECKED_CAST", "DEPRECATION", "REDUNDANT_PROJECTION", "REMOVAL"])
public class KnownHostDao_Impl(
  __db: RoomDatabase,
) : KnownHostDao {
  private val __db: RoomDatabase

  private val __insertAdapterOfKnownHost: EntityInsertAdapter<KnownHost>

  private val __deleteAdapterOfKnownHost: EntityDeleteOrUpdateAdapter<KnownHost>
  init {
    this.__db = __db
    this.__insertAdapterOfKnownHost = object : EntityInsertAdapter<KnownHost>() {
      protected override fun createQuery(): String =
          "INSERT OR REPLACE INTO `known_hosts` (`id`,`host`,`port`,`keyType`,`fingerprintSha256`,`firstSeenAt`,`lastSeenAt`) VALUES (nullif(?, 0),?,?,?,?,?,?)"

      protected override fun bind(statement: SQLiteStatement, entity: KnownHost) {
        statement.bindLong(1, entity.id)
        statement.bindText(2, entity.host)
        statement.bindLong(3, entity.port.toLong())
        statement.bindText(4, entity.keyType)
        statement.bindText(5, entity.fingerprintSha256)
        statement.bindLong(6, entity.firstSeenAt)
        statement.bindLong(7, entity.lastSeenAt)
      }
    }
    this.__deleteAdapterOfKnownHost = object : EntityDeleteOrUpdateAdapter<KnownHost>() {
      protected override fun createQuery(): String = "DELETE FROM `known_hosts` WHERE `id` = ?"

      protected override fun bind(statement: SQLiteStatement, entity: KnownHost) {
        statement.bindLong(1, entity.id)
      }
    }
  }

  public override suspend fun upsert(host: KnownHost): Unit = performSuspending(__db, false, true) {
      _connection ->
    __insertAdapterOfKnownHost.insert(_connection, host)
  }

  public override suspend fun delete(host: KnownHost): Unit = performSuspending(__db, false, true) {
      _connection ->
    __deleteAdapterOfKnownHost.handle(_connection, host)
  }

  public override suspend fun findByHostPort(host: String, port: Int): KnownHost? {
    val _sql: String = "SELECT * FROM known_hosts WHERE host = ? AND port = ? LIMIT 1"
    return performSuspending(__db, true, false) { _connection ->
      val _stmt: SQLiteStatement = _connection.prepare(_sql)
      try {
        var _argIndex: Int = 1
        _stmt.bindText(_argIndex, host)
        _argIndex = 2
        _stmt.bindLong(_argIndex, port.toLong())
        val _columnIndexOfId: Int = getColumnIndexOrThrow(_stmt, "id")
        val _columnIndexOfHost: Int = getColumnIndexOrThrow(_stmt, "host")
        val _columnIndexOfPort: Int = getColumnIndexOrThrow(_stmt, "port")
        val _columnIndexOfKeyType: Int = getColumnIndexOrThrow(_stmt, "keyType")
        val _columnIndexOfFingerprintSha256: Int = getColumnIndexOrThrow(_stmt, "fingerprintSha256")
        val _columnIndexOfFirstSeenAt: Int = getColumnIndexOrThrow(_stmt, "firstSeenAt")
        val _columnIndexOfLastSeenAt: Int = getColumnIndexOrThrow(_stmt, "lastSeenAt")
        val _result: KnownHost?
        if (_stmt.step()) {
          val _tmpId: Long
          _tmpId = _stmt.getLong(_columnIndexOfId)
          val _tmpHost: String
          _tmpHost = _stmt.getText(_columnIndexOfHost)
          val _tmpPort: Int
          _tmpPort = _stmt.getLong(_columnIndexOfPort).toInt()
          val _tmpKeyType: String
          _tmpKeyType = _stmt.getText(_columnIndexOfKeyType)
          val _tmpFingerprintSha256: String
          _tmpFingerprintSha256 = _stmt.getText(_columnIndexOfFingerprintSha256)
          val _tmpFirstSeenAt: Long
          _tmpFirstSeenAt = _stmt.getLong(_columnIndexOfFirstSeenAt)
          val _tmpLastSeenAt: Long
          _tmpLastSeenAt = _stmt.getLong(_columnIndexOfLastSeenAt)
          _result =
              KnownHost(_tmpId,_tmpHost,_tmpPort,_tmpKeyType,_tmpFingerprintSha256,_tmpFirstSeenAt,_tmpLastSeenAt)
        } else {
          _result = null
        }
        _result
      } finally {
        _stmt.close()
      }
    }
  }

  public override suspend fun getAll(): List<KnownHost> {
    val _sql: String = "SELECT * FROM known_hosts ORDER BY host ASC"
    return performSuspending(__db, true, false) { _connection ->
      val _stmt: SQLiteStatement = _connection.prepare(_sql)
      try {
        val _columnIndexOfId: Int = getColumnIndexOrThrow(_stmt, "id")
        val _columnIndexOfHost: Int = getColumnIndexOrThrow(_stmt, "host")
        val _columnIndexOfPort: Int = getColumnIndexOrThrow(_stmt, "port")
        val _columnIndexOfKeyType: Int = getColumnIndexOrThrow(_stmt, "keyType")
        val _columnIndexOfFingerprintSha256: Int = getColumnIndexOrThrow(_stmt, "fingerprintSha256")
        val _columnIndexOfFirstSeenAt: Int = getColumnIndexOrThrow(_stmt, "firstSeenAt")
        val _columnIndexOfLastSeenAt: Int = getColumnIndexOrThrow(_stmt, "lastSeenAt")
        val _result: MutableList<KnownHost> = mutableListOf()
        while (_stmt.step()) {
          val _item: KnownHost
          val _tmpId: Long
          _tmpId = _stmt.getLong(_columnIndexOfId)
          val _tmpHost: String
          _tmpHost = _stmt.getText(_columnIndexOfHost)
          val _tmpPort: Int
          _tmpPort = _stmt.getLong(_columnIndexOfPort).toInt()
          val _tmpKeyType: String
          _tmpKeyType = _stmt.getText(_columnIndexOfKeyType)
          val _tmpFingerprintSha256: String
          _tmpFingerprintSha256 = _stmt.getText(_columnIndexOfFingerprintSha256)
          val _tmpFirstSeenAt: Long
          _tmpFirstSeenAt = _stmt.getLong(_columnIndexOfFirstSeenAt)
          val _tmpLastSeenAt: Long
          _tmpLastSeenAt = _stmt.getLong(_columnIndexOfLastSeenAt)
          _item =
              KnownHost(_tmpId,_tmpHost,_tmpPort,_tmpKeyType,_tmpFingerprintSha256,_tmpFirstSeenAt,_tmpLastSeenAt)
          _result.add(_item)
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
