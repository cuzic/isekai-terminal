package tools.isekai.terminal.data

import android.content.Context
import kotlinx.coroutines.sync.withLock

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

class KeySequenceRepository(private val dao: KeySequenceDao) {
    suspend fun getAll(): List<KeySequence> = dao.getAll()

    // profileId が null の場合は特定プロファイルに紐付けようがないので、全共通打鍵列のみ返す
    // (SnippetRepository.getForProfile と同じ運用)。
    suspend fun getForProfile(profileId: Long?): List<KeySequence> =
        if (profileId == null || profileId == 0L) dao.getAll().filter { it.profileId == null }
        else dao.getForProfile(profileId)

    suspend fun save(keySequence: KeySequence): Long = dao.upsert(keySequence)
    suspend fun delete(keySequence: KeySequence) = dao.delete(keySequence)
    suspend fun findById(id: Long): KeySequence? = dao.findById(id)
}

/**
 * 打鍵列セット(パック)の有効化状態を扱うRepository。パック定義自体
 * ([tools.isekai.terminal.pack.KeySequencePack])はDB行ではなくアプリ同梱の静的データ。
 *
 * グローバル有効化(`profileId=null`)は同一`packId`につき常に高々1行になるよう、
 * upsert前に既存行を検索してから[KeySequencePackInstallationDao.upsert]する
 * (SQLiteのUNIQUE制約は`NULL`列を重複除外の対象外にするため、DB制約には頼らずアプリ側で保証する)。
 */
class KeySequencePackInstallationRepository(private val dao: KeySequencePackInstallationDao) {
    // install() の「既存行を検索してから書き込む」は2つのDB操作にまたがるため、同一プロセス内で
    // 2つのコルーチンが同時にinstall()を呼ぶと両方が existing == null を見てグローバル行を
    // 2件作りうる(codexレビュー指摘)。Mutexで同一プロセス内の競合を防ぐ(複数プロセス/
    // 複数デバイスからの同時書き込みは元々想定しないアプリのため、DB側のpartial unique index
    // までは導入しない)。
    private val installMutex = kotlinx.coroutines.sync.Mutex()

    suspend fun getAll(): List<KeySequencePackInstallation> = dao.getAll()

    suspend fun findGlobal(packId: String): KeySequencePackInstallation? = dao.findGlobal(packId)

    suspend fun findForProfile(packId: String, profileId: Long): KeySequencePackInstallation? =
        dao.findForProfile(packId, profileId)

    /** [profileId]向けの有効なインストールを解決する。プロファイル別installationがあれば
     *  優先し、なければグローバル(profileId=null)installationを使う。両方無ければnull。 */
    suspend fun resolveInstallation(packId: String, profileId: Long?): KeySequencePackInstallation? {
        if (profileId != null) {
            findForProfile(packId, profileId)?.let { return it }
        }
        return findGlobal(packId)
    }

    suspend fun install(
        packId: String,
        version: Int,
        paramValues: Map<String, tools.isekai.terminal.input.KeyStep>,
        profileId: Long? = null,
    ): Long = installMutex.withLock {
        val existing = if (profileId == null) dao.findGlobal(packId) else dao.findForProfile(packId, profileId)
        dao.upsert(
            KeySequencePackInstallation.create(
                packId = packId,
                version = version,
                paramValues = paramValues,
                profileId = profileId,
                id = existing?.id ?: 0,
            )
        )
    }

    suspend fun uninstall(installation: KeySequencePackInstallation) = dao.delete(installation)
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
    val keySequences get() = KeySequenceRepository(db.keySequenceDao())
    val keySequencePackInstallations get() = KeySequencePackInstallationRepository(db.keySequencePackInstallationDao())
}
