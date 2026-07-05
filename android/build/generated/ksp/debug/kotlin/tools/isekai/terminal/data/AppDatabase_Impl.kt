package tools.isekai.terminal.`data`

import androidx.room.InvalidationTracker
import androidx.room.RoomOpenDelegate
import androidx.room.migration.AutoMigrationSpec
import androidx.room.migration.Migration
import androidx.room.util.TableInfo
import androidx.room.util.TableInfo.Companion.read
import androidx.room.util.dropFtsSyncTriggers
import androidx.sqlite.SQLiteConnection
import androidx.sqlite.execSQL
import javax.`annotation`.processing.Generated
import kotlin.Lazy
import kotlin.String
import kotlin.Suppress
import kotlin.collections.List
import kotlin.collections.Map
import kotlin.collections.MutableList
import kotlin.collections.MutableMap
import kotlin.collections.MutableSet
import kotlin.collections.Set
import kotlin.collections.mutableListOf
import kotlin.collections.mutableMapOf
import kotlin.collections.mutableSetOf
import kotlin.reflect.KClass

@Generated(value = ["androidx.room.RoomProcessor"])
@Suppress(names = ["UNCHECKED_CAST", "DEPRECATION", "REDUNDANT_PROJECTION", "REMOVAL"])
public class AppDatabase_Impl : AppDatabase() {
  private val _knownHostDao: Lazy<KnownHostDao> = lazy {
    KnownHostDao_Impl(this)
  }

  private val _connectionProfileDao: Lazy<ConnectionProfileDao> = lazy {
    ConnectionProfileDao_Impl(this)
  }

  private val _keyEntryDao: Lazy<KeyEntryDao> = lazy {
    KeyEntryDao_Impl(this)
  }

  private val _snippetDao: Lazy<SnippetDao> = lazy {
    SnippetDao_Impl(this)
  }

  protected override fun createOpenDelegate(): RoomOpenDelegate {
    val _openDelegate: RoomOpenDelegate = object : RoomOpenDelegate(17,
        "e2da386977336a8030cf826476e99ff6", "b5af1b679e9944a93fd9bc6bfa8ce0ad") {
      public override fun createAllTables(connection: SQLiteConnection) {
        connection.execSQL("CREATE TABLE IF NOT EXISTS `known_hosts` (`id` INTEGER PRIMARY KEY AUTOINCREMENT NOT NULL, `host` TEXT NOT NULL, `port` INTEGER NOT NULL, `keyType` TEXT NOT NULL, `fingerprintSha256` TEXT NOT NULL, `firstSeenAt` INTEGER NOT NULL, `lastSeenAt` INTEGER NOT NULL)")
        connection.execSQL("CREATE UNIQUE INDEX IF NOT EXISTS `index_known_hosts_host_port` ON `known_hosts` (`host`, `port`)")
        connection.execSQL("CREATE TABLE IF NOT EXISTS `connection_profiles` (`id` INTEGER PRIMARY KEY AUTOINCREMENT NOT NULL, `label` TEXT NOT NULL, `host` TEXT NOT NULL, `port` INTEGER NOT NULL, `username` TEXT NOT NULL, `authType` TEXT NOT NULL, `keyId` INTEGER, `sort_order` INTEGER NOT NULL, `use_tsshd` INTEGER NOT NULL, `tsshd_port` INTEGER NOT NULL, `enable_agent_forward` INTEGER NOT NULL, `transport_preference` TEXT NOT NULL, `direct_address` TEXT, `enable_physical_multipath` INTEGER NOT NULL, `cellular_remote_address` TEXT, `enable_upstream_failover` INTEGER NOT NULL, `post_connect_commands` TEXT, `forwards` TEXT NOT NULL DEFAULT '[]', `jump_host` TEXT, `jump_port` INTEGER NOT NULL, `jump_username` TEXT, `jump_auth_type` TEXT, `jump_key_id` INTEGER, `stun_server` TEXT, `relay_addr` TEXT, `relay_sni` TEXT, `relay_jwt` TEXT, `allow_non_loopback_forward_bind` INTEGER NOT NULL, `theme_name` TEXT, `helper_bind_port` INTEGER)")
        connection.execSQL("CREATE TABLE IF NOT EXISTS `key_entries` (`id` INTEGER PRIMARY KEY AUTOINCREMENT NOT NULL, `label` TEXT NOT NULL, `publicKey` TEXT NOT NULL, `encryptedPrivateKeyPath` TEXT NOT NULL, `kekAlias` TEXT NOT NULL, `createdAt` INTEGER NOT NULL)")
        connection.execSQL("CREATE TABLE IF NOT EXISTS `snippets` (`id` INTEGER PRIMARY KEY AUTOINCREMENT NOT NULL, `label` TEXT NOT NULL, `command` TEXT NOT NULL, `sort_order` INTEGER NOT NULL, `profile_id` INTEGER, `append_newline` INTEGER NOT NULL)")
        connection.execSQL("CREATE TABLE IF NOT EXISTS room_master_table (id INTEGER PRIMARY KEY,identity_hash TEXT)")
        connection.execSQL("INSERT OR REPLACE INTO room_master_table (id,identity_hash) VALUES(42, 'e2da386977336a8030cf826476e99ff6')")
      }

      public override fun dropAllTables(connection: SQLiteConnection) {
        connection.execSQL("DROP TABLE IF EXISTS `known_hosts`")
        connection.execSQL("DROP TABLE IF EXISTS `connection_profiles`")
        connection.execSQL("DROP TABLE IF EXISTS `key_entries`")
        connection.execSQL("DROP TABLE IF EXISTS `snippets`")
      }

      public override fun onCreate(connection: SQLiteConnection) {
      }

      public override fun onOpen(connection: SQLiteConnection) {
        internalInitInvalidationTracker(connection)
      }

      public override fun onPreMigrate(connection: SQLiteConnection) {
        dropFtsSyncTriggers(connection)
      }

      public override fun onPostMigrate(connection: SQLiteConnection) {
      }

      public override fun onValidateSchema(connection: SQLiteConnection):
          RoomOpenDelegate.ValidationResult {
        val _columnsKnownHosts: MutableMap<String, TableInfo.Column> = mutableMapOf()
        _columnsKnownHosts.put("id", TableInfo.Column("id", "INTEGER", true, 1, null,
            TableInfo.CREATED_FROM_ENTITY))
        _columnsKnownHosts.put("host", TableInfo.Column("host", "TEXT", true, 0, null,
            TableInfo.CREATED_FROM_ENTITY))
        _columnsKnownHosts.put("port", TableInfo.Column("port", "INTEGER", true, 0, null,
            TableInfo.CREATED_FROM_ENTITY))
        _columnsKnownHosts.put("keyType", TableInfo.Column("keyType", "TEXT", true, 0, null,
            TableInfo.CREATED_FROM_ENTITY))
        _columnsKnownHosts.put("fingerprintSha256", TableInfo.Column("fingerprintSha256", "TEXT",
            true, 0, null, TableInfo.CREATED_FROM_ENTITY))
        _columnsKnownHosts.put("firstSeenAt", TableInfo.Column("firstSeenAt", "INTEGER", true, 0,
            null, TableInfo.CREATED_FROM_ENTITY))
        _columnsKnownHosts.put("lastSeenAt", TableInfo.Column("lastSeenAt", "INTEGER", true, 0,
            null, TableInfo.CREATED_FROM_ENTITY))
        val _foreignKeysKnownHosts: MutableSet<TableInfo.ForeignKey> = mutableSetOf()
        val _indicesKnownHosts: MutableSet<TableInfo.Index> = mutableSetOf()
        _indicesKnownHosts.add(TableInfo.Index("index_known_hosts_host_port", true, listOf("host",
            "port"), listOf("ASC", "ASC")))
        val _infoKnownHosts: TableInfo = TableInfo("known_hosts", _columnsKnownHosts,
            _foreignKeysKnownHosts, _indicesKnownHosts)
        val _existingKnownHosts: TableInfo = read(connection, "known_hosts")
        if (!_infoKnownHosts.equals(_existingKnownHosts)) {
          return RoomOpenDelegate.ValidationResult(false, """
              |known_hosts(tools.isekai.terminal.data.KnownHost).
              | Expected:
              |""".trimMargin() + _infoKnownHosts + """
              |
              | Found:
              |""".trimMargin() + _existingKnownHosts)
        }
        val _columnsConnectionProfiles: MutableMap<String, TableInfo.Column> = mutableMapOf()
        _columnsConnectionProfiles.put("id", TableInfo.Column("id", "INTEGER", true, 1, null,
            TableInfo.CREATED_FROM_ENTITY))
        _columnsConnectionProfiles.put("label", TableInfo.Column("label", "TEXT", true, 0, null,
            TableInfo.CREATED_FROM_ENTITY))
        _columnsConnectionProfiles.put("host", TableInfo.Column("host", "TEXT", true, 0, null,
            TableInfo.CREATED_FROM_ENTITY))
        _columnsConnectionProfiles.put("port", TableInfo.Column("port", "INTEGER", true, 0, null,
            TableInfo.CREATED_FROM_ENTITY))
        _columnsConnectionProfiles.put("username", TableInfo.Column("username", "TEXT", true, 0,
            null, TableInfo.CREATED_FROM_ENTITY))
        _columnsConnectionProfiles.put("authType", TableInfo.Column("authType", "TEXT", true, 0,
            null, TableInfo.CREATED_FROM_ENTITY))
        _columnsConnectionProfiles.put("keyId", TableInfo.Column("keyId", "INTEGER", false, 0, null,
            TableInfo.CREATED_FROM_ENTITY))
        _columnsConnectionProfiles.put("sort_order", TableInfo.Column("sort_order", "INTEGER", true,
            0, null, TableInfo.CREATED_FROM_ENTITY))
        _columnsConnectionProfiles.put("use_tsshd", TableInfo.Column("use_tsshd", "INTEGER", true,
            0, null, TableInfo.CREATED_FROM_ENTITY))
        _columnsConnectionProfiles.put("tsshd_port", TableInfo.Column("tsshd_port", "INTEGER", true,
            0, null, TableInfo.CREATED_FROM_ENTITY))
        _columnsConnectionProfiles.put("enable_agent_forward",
            TableInfo.Column("enable_agent_forward", "INTEGER", true, 0, null,
            TableInfo.CREATED_FROM_ENTITY))
        _columnsConnectionProfiles.put("transport_preference",
            TableInfo.Column("transport_preference", "TEXT", true, 0, null,
            TableInfo.CREATED_FROM_ENTITY))
        _columnsConnectionProfiles.put("direct_address", TableInfo.Column("direct_address", "TEXT",
            false, 0, null, TableInfo.CREATED_FROM_ENTITY))
        _columnsConnectionProfiles.put("enable_physical_multipath",
            TableInfo.Column("enable_physical_multipath", "INTEGER", true, 0, null,
            TableInfo.CREATED_FROM_ENTITY))
        _columnsConnectionProfiles.put("cellular_remote_address",
            TableInfo.Column("cellular_remote_address", "TEXT", false, 0, null,
            TableInfo.CREATED_FROM_ENTITY))
        _columnsConnectionProfiles.put("enable_upstream_failover",
            TableInfo.Column("enable_upstream_failover", "INTEGER", true, 0, null,
            TableInfo.CREATED_FROM_ENTITY))
        _columnsConnectionProfiles.put("post_connect_commands",
            TableInfo.Column("post_connect_commands", "TEXT", false, 0, null,
            TableInfo.CREATED_FROM_ENTITY))
        _columnsConnectionProfiles.put("forwards", TableInfo.Column("forwards", "TEXT", true, 0,
            "'[]'", TableInfo.CREATED_FROM_ENTITY))
        _columnsConnectionProfiles.put("jump_host", TableInfo.Column("jump_host", "TEXT", false, 0,
            null, TableInfo.CREATED_FROM_ENTITY))
        _columnsConnectionProfiles.put("jump_port", TableInfo.Column("jump_port", "INTEGER", true,
            0, null, TableInfo.CREATED_FROM_ENTITY))
        _columnsConnectionProfiles.put("jump_username", TableInfo.Column("jump_username", "TEXT",
            false, 0, null, TableInfo.CREATED_FROM_ENTITY))
        _columnsConnectionProfiles.put("jump_auth_type", TableInfo.Column("jump_auth_type", "TEXT",
            false, 0, null, TableInfo.CREATED_FROM_ENTITY))
        _columnsConnectionProfiles.put("jump_key_id", TableInfo.Column("jump_key_id", "INTEGER",
            false, 0, null, TableInfo.CREATED_FROM_ENTITY))
        _columnsConnectionProfiles.put("stun_server", TableInfo.Column("stun_server", "TEXT", false,
            0, null, TableInfo.CREATED_FROM_ENTITY))
        _columnsConnectionProfiles.put("relay_addr", TableInfo.Column("relay_addr", "TEXT", false,
            0, null, TableInfo.CREATED_FROM_ENTITY))
        _columnsConnectionProfiles.put("relay_sni", TableInfo.Column("relay_sni", "TEXT", false, 0,
            null, TableInfo.CREATED_FROM_ENTITY))
        _columnsConnectionProfiles.put("relay_jwt", TableInfo.Column("relay_jwt", "TEXT", false, 0,
            null, TableInfo.CREATED_FROM_ENTITY))
        _columnsConnectionProfiles.put("allow_non_loopback_forward_bind",
            TableInfo.Column("allow_non_loopback_forward_bind", "INTEGER", true, 0, null,
            TableInfo.CREATED_FROM_ENTITY))
        _columnsConnectionProfiles.put("theme_name", TableInfo.Column("theme_name", "TEXT", false,
            0, null, TableInfo.CREATED_FROM_ENTITY))
        _columnsConnectionProfiles.put("helper_bind_port", TableInfo.Column("helper_bind_port",
            "INTEGER", false, 0, null, TableInfo.CREATED_FROM_ENTITY))
        val _foreignKeysConnectionProfiles: MutableSet<TableInfo.ForeignKey> = mutableSetOf()
        val _indicesConnectionProfiles: MutableSet<TableInfo.Index> = mutableSetOf()
        val _infoConnectionProfiles: TableInfo = TableInfo("connection_profiles",
            _columnsConnectionProfiles, _foreignKeysConnectionProfiles, _indicesConnectionProfiles)
        val _existingConnectionProfiles: TableInfo = read(connection, "connection_profiles")
        if (!_infoConnectionProfiles.equals(_existingConnectionProfiles)) {
          return RoomOpenDelegate.ValidationResult(false, """
              |connection_profiles(tools.isekai.terminal.data.ConnectionProfile).
              | Expected:
              |""".trimMargin() + _infoConnectionProfiles + """
              |
              | Found:
              |""".trimMargin() + _existingConnectionProfiles)
        }
        val _columnsKeyEntries: MutableMap<String, TableInfo.Column> = mutableMapOf()
        _columnsKeyEntries.put("id", TableInfo.Column("id", "INTEGER", true, 1, null,
            TableInfo.CREATED_FROM_ENTITY))
        _columnsKeyEntries.put("label", TableInfo.Column("label", "TEXT", true, 0, null,
            TableInfo.CREATED_FROM_ENTITY))
        _columnsKeyEntries.put("publicKey", TableInfo.Column("publicKey", "TEXT", true, 0, null,
            TableInfo.CREATED_FROM_ENTITY))
        _columnsKeyEntries.put("encryptedPrivateKeyPath",
            TableInfo.Column("encryptedPrivateKeyPath", "TEXT", true, 0, null,
            TableInfo.CREATED_FROM_ENTITY))
        _columnsKeyEntries.put("kekAlias", TableInfo.Column("kekAlias", "TEXT", true, 0, null,
            TableInfo.CREATED_FROM_ENTITY))
        _columnsKeyEntries.put("createdAt", TableInfo.Column("createdAt", "INTEGER", true, 0, null,
            TableInfo.CREATED_FROM_ENTITY))
        val _foreignKeysKeyEntries: MutableSet<TableInfo.ForeignKey> = mutableSetOf()
        val _indicesKeyEntries: MutableSet<TableInfo.Index> = mutableSetOf()
        val _infoKeyEntries: TableInfo = TableInfo("key_entries", _columnsKeyEntries,
            _foreignKeysKeyEntries, _indicesKeyEntries)
        val _existingKeyEntries: TableInfo = read(connection, "key_entries")
        if (!_infoKeyEntries.equals(_existingKeyEntries)) {
          return RoomOpenDelegate.ValidationResult(false, """
              |key_entries(tools.isekai.terminal.data.KeyEntry).
              | Expected:
              |""".trimMargin() + _infoKeyEntries + """
              |
              | Found:
              |""".trimMargin() + _existingKeyEntries)
        }
        val _columnsSnippets: MutableMap<String, TableInfo.Column> = mutableMapOf()
        _columnsSnippets.put("id", TableInfo.Column("id", "INTEGER", true, 1, null,
            TableInfo.CREATED_FROM_ENTITY))
        _columnsSnippets.put("label", TableInfo.Column("label", "TEXT", true, 0, null,
            TableInfo.CREATED_FROM_ENTITY))
        _columnsSnippets.put("command", TableInfo.Column("command", "TEXT", true, 0, null,
            TableInfo.CREATED_FROM_ENTITY))
        _columnsSnippets.put("sort_order", TableInfo.Column("sort_order", "INTEGER", true, 0, null,
            TableInfo.CREATED_FROM_ENTITY))
        _columnsSnippets.put("profile_id", TableInfo.Column("profile_id", "INTEGER", false, 0, null,
            TableInfo.CREATED_FROM_ENTITY))
        _columnsSnippets.put("append_newline", TableInfo.Column("append_newline", "INTEGER", true,
            0, null, TableInfo.CREATED_FROM_ENTITY))
        val _foreignKeysSnippets: MutableSet<TableInfo.ForeignKey> = mutableSetOf()
        val _indicesSnippets: MutableSet<TableInfo.Index> = mutableSetOf()
        val _infoSnippets: TableInfo = TableInfo("snippets", _columnsSnippets, _foreignKeysSnippets,
            _indicesSnippets)
        val _existingSnippets: TableInfo = read(connection, "snippets")
        if (!_infoSnippets.equals(_existingSnippets)) {
          return RoomOpenDelegate.ValidationResult(false, """
              |snippets(tools.isekai.terminal.data.Snippet).
              | Expected:
              |""".trimMargin() + _infoSnippets + """
              |
              | Found:
              |""".trimMargin() + _existingSnippets)
        }
        return RoomOpenDelegate.ValidationResult(true, null)
      }
    }
    return _openDelegate
  }

  protected override fun createInvalidationTracker(): InvalidationTracker {
    val _shadowTablesMap: MutableMap<String, String> = mutableMapOf()
    val _viewTables: MutableMap<String, Set<String>> = mutableMapOf()
    return InvalidationTracker(this, _shadowTablesMap, _viewTables, "known_hosts",
        "connection_profiles", "key_entries", "snippets")
  }

  public override fun clearAllTables() {
    super.performClear(false, "known_hosts", "connection_profiles", "key_entries", "snippets")
  }

  protected override fun getRequiredTypeConverterClasses(): Map<KClass<*>, List<KClass<*>>> {
    val _typeConvertersMap: MutableMap<KClass<*>, List<KClass<*>>> = mutableMapOf()
    _typeConvertersMap.put(KnownHostDao::class, KnownHostDao_Impl.getRequiredConverters())
    _typeConvertersMap.put(ConnectionProfileDao::class,
        ConnectionProfileDao_Impl.getRequiredConverters())
    _typeConvertersMap.put(KeyEntryDao::class, KeyEntryDao_Impl.getRequiredConverters())
    _typeConvertersMap.put(SnippetDao::class, SnippetDao_Impl.getRequiredConverters())
    return _typeConvertersMap
  }

  public override fun getRequiredAutoMigrationSpecClasses(): Set<KClass<out AutoMigrationSpec>> {
    val _autoMigrationSpecsSet: MutableSet<KClass<out AutoMigrationSpec>> = mutableSetOf()
    return _autoMigrationSpecsSet
  }

  public override
      fun createAutoMigrations(autoMigrationSpecs: Map<KClass<out AutoMigrationSpec>, AutoMigrationSpec>):
      List<Migration> {
    val _autoMigrations: MutableList<Migration> = mutableListOf()
    return _autoMigrations
  }

  public override fun knownHostDao(): KnownHostDao = _knownHostDao.value

  public override fun connectionProfileDao(): ConnectionProfileDao = _connectionProfileDao.value

  public override fun keyEntryDao(): KeyEntryDao = _keyEntryDao.value

  public override fun snippetDao(): SnippetDao = _snippetDao.value
}
