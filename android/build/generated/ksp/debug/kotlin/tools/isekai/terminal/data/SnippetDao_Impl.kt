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

@Generated(value = ["androidx.room.RoomProcessor"])
@Suppress(names = ["UNCHECKED_CAST", "DEPRECATION", "REDUNDANT_PROJECTION", "REMOVAL"])
public class SnippetDao_Impl(
  __db: RoomDatabase,
) : SnippetDao {
  private val __db: RoomDatabase

  private val __insertAdapterOfSnippet: EntityInsertAdapter<Snippet>

  private val __deleteAdapterOfSnippet: EntityDeleteOrUpdateAdapter<Snippet>
  init {
    this.__db = __db
    this.__insertAdapterOfSnippet = object : EntityInsertAdapter<Snippet>() {
      protected override fun createQuery(): String =
          "INSERT OR REPLACE INTO `snippets` (`id`,`label`,`command`,`sort_order`,`profile_id`,`append_newline`) VALUES (nullif(?, 0),?,?,?,?,?)"

      protected override fun bind(statement: SQLiteStatement, entity: Snippet) {
        statement.bindLong(1, entity.id)
        statement.bindText(2, entity.label)
        statement.bindText(3, entity.command)
        statement.bindLong(4, entity.sortOrder.toLong())
        val _tmpProfileId: Long? = entity.profileId
        if (_tmpProfileId == null) {
          statement.bindNull(5)
        } else {
          statement.bindLong(5, _tmpProfileId)
        }
        val _tmp: Int = if (entity.appendNewline) 1 else 0
        statement.bindLong(6, _tmp.toLong())
      }
    }
    this.__deleteAdapterOfSnippet = object : EntityDeleteOrUpdateAdapter<Snippet>() {
      protected override fun createQuery(): String = "DELETE FROM `snippets` WHERE `id` = ?"

      protected override fun bind(statement: SQLiteStatement, entity: Snippet) {
        statement.bindLong(1, entity.id)
      }
    }
  }

  public override suspend fun upsert(snippet: Snippet): Long = performSuspending(__db, false, true)
      { _connection ->
    val _result: Long = __insertAdapterOfSnippet.insertAndReturnId(_connection, snippet)
    _result
  }

  public override suspend fun delete(snippet: Snippet): Unit = performSuspending(__db, false, true)
      { _connection ->
    __deleteAdapterOfSnippet.handle(_connection, snippet)
  }

  public override suspend fun getAll(): List<Snippet> {
    val _sql: String = "SELECT * FROM snippets ORDER BY sort_order ASC, label ASC"
    return performSuspending(__db, true, false) { _connection ->
      val _stmt: SQLiteStatement = _connection.prepare(_sql)
      try {
        val _columnIndexOfId: Int = getColumnIndexOrThrow(_stmt, "id")
        val _columnIndexOfLabel: Int = getColumnIndexOrThrow(_stmt, "label")
        val _columnIndexOfCommand: Int = getColumnIndexOrThrow(_stmt, "command")
        val _columnIndexOfSortOrder: Int = getColumnIndexOrThrow(_stmt, "sort_order")
        val _columnIndexOfProfileId: Int = getColumnIndexOrThrow(_stmt, "profile_id")
        val _columnIndexOfAppendNewline: Int = getColumnIndexOrThrow(_stmt, "append_newline")
        val _result: MutableList<Snippet> = mutableListOf()
        while (_stmt.step()) {
          val _item: Snippet
          val _tmpId: Long
          _tmpId = _stmt.getLong(_columnIndexOfId)
          val _tmpLabel: String
          _tmpLabel = _stmt.getText(_columnIndexOfLabel)
          val _tmpCommand: String
          _tmpCommand = _stmt.getText(_columnIndexOfCommand)
          val _tmpSortOrder: Int
          _tmpSortOrder = _stmt.getLong(_columnIndexOfSortOrder).toInt()
          val _tmpProfileId: Long?
          if (_stmt.isNull(_columnIndexOfProfileId)) {
            _tmpProfileId = null
          } else {
            _tmpProfileId = _stmt.getLong(_columnIndexOfProfileId)
          }
          val _tmpAppendNewline: Boolean
          val _tmp: Int
          _tmp = _stmt.getLong(_columnIndexOfAppendNewline).toInt()
          _tmpAppendNewline = _tmp != 0
          _item =
              Snippet(_tmpId,_tmpLabel,_tmpCommand,_tmpSortOrder,_tmpProfileId,_tmpAppendNewline)
          _result.add(_item)
        }
        _result
      } finally {
        _stmt.close()
      }
    }
  }

  public override suspend fun getForProfile(profileId: Long): List<Snippet> {
    val _sql: String =
        "SELECT * FROM snippets WHERE profile_id IS NULL OR profile_id = ? ORDER BY sort_order ASC, label ASC"
    return performSuspending(__db, true, false) { _connection ->
      val _stmt: SQLiteStatement = _connection.prepare(_sql)
      try {
        var _argIndex: Int = 1
        _stmt.bindLong(_argIndex, profileId)
        val _columnIndexOfId: Int = getColumnIndexOrThrow(_stmt, "id")
        val _columnIndexOfLabel: Int = getColumnIndexOrThrow(_stmt, "label")
        val _columnIndexOfCommand: Int = getColumnIndexOrThrow(_stmt, "command")
        val _columnIndexOfSortOrder: Int = getColumnIndexOrThrow(_stmt, "sort_order")
        val _columnIndexOfProfileId: Int = getColumnIndexOrThrow(_stmt, "profile_id")
        val _columnIndexOfAppendNewline: Int = getColumnIndexOrThrow(_stmt, "append_newline")
        val _result: MutableList<Snippet> = mutableListOf()
        while (_stmt.step()) {
          val _item: Snippet
          val _tmpId: Long
          _tmpId = _stmt.getLong(_columnIndexOfId)
          val _tmpLabel: String
          _tmpLabel = _stmt.getText(_columnIndexOfLabel)
          val _tmpCommand: String
          _tmpCommand = _stmt.getText(_columnIndexOfCommand)
          val _tmpSortOrder: Int
          _tmpSortOrder = _stmt.getLong(_columnIndexOfSortOrder).toInt()
          val _tmpProfileId: Long?
          if (_stmt.isNull(_columnIndexOfProfileId)) {
            _tmpProfileId = null
          } else {
            _tmpProfileId = _stmt.getLong(_columnIndexOfProfileId)
          }
          val _tmpAppendNewline: Boolean
          val _tmp: Int
          _tmp = _stmt.getLong(_columnIndexOfAppendNewline).toInt()
          _tmpAppendNewline = _tmp != 0
          _item =
              Snippet(_tmpId,_tmpLabel,_tmpCommand,_tmpSortOrder,_tmpProfileId,_tmpAppendNewline)
          _result.add(_item)
        }
        _result
      } finally {
        _stmt.close()
      }
    }
  }

  public override suspend fun findById(id: Long): Snippet? {
    val _sql: String = "SELECT * FROM snippets WHERE id = ? LIMIT 1"
    return performSuspending(__db, true, false) { _connection ->
      val _stmt: SQLiteStatement = _connection.prepare(_sql)
      try {
        var _argIndex: Int = 1
        _stmt.bindLong(_argIndex, id)
        val _columnIndexOfId: Int = getColumnIndexOrThrow(_stmt, "id")
        val _columnIndexOfLabel: Int = getColumnIndexOrThrow(_stmt, "label")
        val _columnIndexOfCommand: Int = getColumnIndexOrThrow(_stmt, "command")
        val _columnIndexOfSortOrder: Int = getColumnIndexOrThrow(_stmt, "sort_order")
        val _columnIndexOfProfileId: Int = getColumnIndexOrThrow(_stmt, "profile_id")
        val _columnIndexOfAppendNewline: Int = getColumnIndexOrThrow(_stmt, "append_newline")
        val _result: Snippet?
        if (_stmt.step()) {
          val _tmpId: Long
          _tmpId = _stmt.getLong(_columnIndexOfId)
          val _tmpLabel: String
          _tmpLabel = _stmt.getText(_columnIndexOfLabel)
          val _tmpCommand: String
          _tmpCommand = _stmt.getText(_columnIndexOfCommand)
          val _tmpSortOrder: Int
          _tmpSortOrder = _stmt.getLong(_columnIndexOfSortOrder).toInt()
          val _tmpProfileId: Long?
          if (_stmt.isNull(_columnIndexOfProfileId)) {
            _tmpProfileId = null
          } else {
            _tmpProfileId = _stmt.getLong(_columnIndexOfProfileId)
          }
          val _tmpAppendNewline: Boolean
          val _tmp: Int
          _tmp = _stmt.getLong(_columnIndexOfAppendNewline).toInt()
          _tmpAppendNewline = _tmp != 0
          _result =
              Snippet(_tmpId,_tmpLabel,_tmpCommand,_tmpSortOrder,_tmpProfileId,_tmpAppendNewline)
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
