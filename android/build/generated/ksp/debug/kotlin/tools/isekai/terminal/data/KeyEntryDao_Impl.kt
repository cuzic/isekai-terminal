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
public class KeyEntryDao_Impl(
  __db: RoomDatabase,
) : KeyEntryDao {
  private val __db: RoomDatabase

  private val __insertAdapterOfKeyEntry: EntityInsertAdapter<KeyEntry>

  private val __deleteAdapterOfKeyEntry: EntityDeleteOrUpdateAdapter<KeyEntry>
  init {
    this.__db = __db
    this.__insertAdapterOfKeyEntry = object : EntityInsertAdapter<KeyEntry>() {
      protected override fun createQuery(): String =
          "INSERT OR REPLACE INTO `key_entries` (`id`,`label`,`publicKey`,`encryptedPrivateKeyPath`,`kekAlias`,`createdAt`) VALUES (nullif(?, 0),?,?,?,?,?)"

      protected override fun bind(statement: SQLiteStatement, entity: KeyEntry) {
        statement.bindLong(1, entity.id)
        statement.bindText(2, entity.label)
        statement.bindText(3, entity.publicKey)
        statement.bindText(4, entity.encryptedPrivateKeyPath)
        statement.bindText(5, entity.kekAlias)
        statement.bindLong(6, entity.createdAt)
      }
    }
    this.__deleteAdapterOfKeyEntry = object : EntityDeleteOrUpdateAdapter<KeyEntry>() {
      protected override fun createQuery(): String = "DELETE FROM `key_entries` WHERE `id` = ?"

      protected override fun bind(statement: SQLiteStatement, entity: KeyEntry) {
        statement.bindLong(1, entity.id)
      }
    }
  }

  public override suspend fun upsert(key: KeyEntry): Long = performSuspending(__db, false, true) {
      _connection ->
    val _result: Long = __insertAdapterOfKeyEntry.insertAndReturnId(_connection, key)
    _result
  }

  public override suspend fun delete(key: KeyEntry): Unit = performSuspending(__db, false, true) {
      _connection ->
    __deleteAdapterOfKeyEntry.handle(_connection, key)
  }

  public override suspend fun getAll(): List<KeyEntry> {
    val _sql: String = "SELECT * FROM key_entries ORDER BY label ASC"
    return performSuspending(__db, true, false) { _connection ->
      val _stmt: SQLiteStatement = _connection.prepare(_sql)
      try {
        val _columnIndexOfId: Int = getColumnIndexOrThrow(_stmt, "id")
        val _columnIndexOfLabel: Int = getColumnIndexOrThrow(_stmt, "label")
        val _columnIndexOfPublicKey: Int = getColumnIndexOrThrow(_stmt, "publicKey")
        val _columnIndexOfEncryptedPrivateKeyPath: Int = getColumnIndexOrThrow(_stmt,
            "encryptedPrivateKeyPath")
        val _columnIndexOfKekAlias: Int = getColumnIndexOrThrow(_stmt, "kekAlias")
        val _columnIndexOfCreatedAt: Int = getColumnIndexOrThrow(_stmt, "createdAt")
        val _result: MutableList<KeyEntry> = mutableListOf()
        while (_stmt.step()) {
          val _item: KeyEntry
          val _tmpId: Long
          _tmpId = _stmt.getLong(_columnIndexOfId)
          val _tmpLabel: String
          _tmpLabel = _stmt.getText(_columnIndexOfLabel)
          val _tmpPublicKey: String
          _tmpPublicKey = _stmt.getText(_columnIndexOfPublicKey)
          val _tmpEncryptedPrivateKeyPath: String
          _tmpEncryptedPrivateKeyPath = _stmt.getText(_columnIndexOfEncryptedPrivateKeyPath)
          val _tmpKekAlias: String
          _tmpKekAlias = _stmt.getText(_columnIndexOfKekAlias)
          val _tmpCreatedAt: Long
          _tmpCreatedAt = _stmt.getLong(_columnIndexOfCreatedAt)
          _item =
              KeyEntry(_tmpId,_tmpLabel,_tmpPublicKey,_tmpEncryptedPrivateKeyPath,_tmpKekAlias,_tmpCreatedAt)
          _result.add(_item)
        }
        _result
      } finally {
        _stmt.close()
      }
    }
  }

  public override suspend fun findById(id: Long): KeyEntry? {
    val _sql: String = "SELECT * FROM key_entries WHERE id = ? LIMIT 1"
    return performSuspending(__db, true, false) { _connection ->
      val _stmt: SQLiteStatement = _connection.prepare(_sql)
      try {
        var _argIndex: Int = 1
        _stmt.bindLong(_argIndex, id)
        val _columnIndexOfId: Int = getColumnIndexOrThrow(_stmt, "id")
        val _columnIndexOfLabel: Int = getColumnIndexOrThrow(_stmt, "label")
        val _columnIndexOfPublicKey: Int = getColumnIndexOrThrow(_stmt, "publicKey")
        val _columnIndexOfEncryptedPrivateKeyPath: Int = getColumnIndexOrThrow(_stmt,
            "encryptedPrivateKeyPath")
        val _columnIndexOfKekAlias: Int = getColumnIndexOrThrow(_stmt, "kekAlias")
        val _columnIndexOfCreatedAt: Int = getColumnIndexOrThrow(_stmt, "createdAt")
        val _result: KeyEntry?
        if (_stmt.step()) {
          val _tmpId: Long
          _tmpId = _stmt.getLong(_columnIndexOfId)
          val _tmpLabel: String
          _tmpLabel = _stmt.getText(_columnIndexOfLabel)
          val _tmpPublicKey: String
          _tmpPublicKey = _stmt.getText(_columnIndexOfPublicKey)
          val _tmpEncryptedPrivateKeyPath: String
          _tmpEncryptedPrivateKeyPath = _stmt.getText(_columnIndexOfEncryptedPrivateKeyPath)
          val _tmpKekAlias: String
          _tmpKekAlias = _stmt.getText(_columnIndexOfKekAlias)
          val _tmpCreatedAt: Long
          _tmpCreatedAt = _stmt.getLong(_columnIndexOfCreatedAt)
          _result =
              KeyEntry(_tmpId,_tmpLabel,_tmpPublicKey,_tmpEncryptedPrivateKeyPath,_tmpKekAlias,_tmpCreatedAt)
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
