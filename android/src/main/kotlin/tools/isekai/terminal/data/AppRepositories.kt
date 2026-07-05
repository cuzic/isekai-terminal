package tools.isekai.terminal.data

import android.content.Context

class KnownHostRepository(private val dao: KnownHostDao) {
    // Returns: null=unknown, same fingerprint=trusted, different=CHANGED
    suspend fun verify(host: String, port: Int, fingerprint: String): HostKeyStatus {
        val stored = dao.findByHostPort(host, port) ?: return HostKeyStatus.Unknown
        return if (stored.fingerprintSha256 == fingerprint) HostKeyStatus.Trusted
        else HostKeyStatus.Changed(stored.fingerprintSha256)
    }

    suspend fun trust(host: String, port: Int, keyType: String, fingerprint: String) {
        val now = System.currentTimeMillis()
        val existing = dao.findByHostPort(host, port)
        dao.upsert(KnownHost(
            id = existing?.id ?: 0,
            host = host,
            port = port,
            keyType = keyType,
            fingerprintSha256 = fingerprint,
            firstSeenAt = existing?.firstSeenAt ?: now,
            lastSeenAt = now,
        ))
    }

    suspend fun forget(host: String, port: Int) {
        dao.findByHostPort(host, port)?.let { dao.delete(it) }
    }
}

sealed class HostKeyStatus {
    object Unknown : HostKeyStatus()
    object Trusted : HostKeyStatus()
    data class Changed(val oldFingerprint: String) : HostKeyStatus()
}

class ConnectionProfileRepository(private val dao: ConnectionProfileDao) {
    suspend fun getAll(): List<ConnectionProfile> = dao.getAll()
    suspend fun save(profile: ConnectionProfile): Long = dao.upsert(profile)
    suspend fun delete(profile: ConnectionProfile) = dao.delete(profile)
    suspend fun findById(id: Long): ConnectionProfile? = dao.findById(id)
}

class KeyEntryRepository(private val dao: KeyEntryDao) {
    suspend fun getAll(): List<KeyEntry> = dao.getAll()
    suspend fun save(key: KeyEntry): Long = dao.upsert(key)
    suspend fun delete(key: KeyEntry) = dao.delete(key)
    suspend fun findById(id: Long): KeyEntry? = dao.findById(id)
}

class SnippetRepository(private val dao: SnippetDao) {
    suspend fun getAll(): List<Snippet> = dao.getAll()

    // profileId が null の場合は特定プロファイルに紐付けようがないので、全共通スニペットのみ返す。
    suspend fun getForProfile(profileId: Long?): List<Snippet> =
        if (profileId == null || profileId == 0L) dao.getAll().filter { it.profileId == null }
        else dao.getForProfile(profileId)

    suspend fun save(snippet: Snippet): Long = dao.upsert(snippet)
    suspend fun delete(snippet: Snippet) = dao.delete(snippet)
    suspend fun findById(id: Long): Snippet? = dao.findById(id)
}

object Repositories {
    private var _db: AppDatabase? = null

    fun init(context: Context) {
        _db = AppDatabase.getInstance(context)
    }

    val db get() = _db ?: error("Repositories.init() not called")
    val knownHosts get() = KnownHostRepository(db.knownHostDao())
    val profiles get() = ConnectionProfileRepository(db.connectionProfileDao())
    val keys get() = KeyEntryRepository(db.keyEntryDao())
    val snippets get() = SnippetRepository(db.snippetDao())
}
