package tools.isekai.terminal.session

import java.io.File
import kotlinx.coroutines.sync.Mutex
import kotlinx.coroutines.sync.withLock
import org.json.JSONArray
import org.json.JSONException
import org.json.JSONObject
import tools.isekai.terminal.util.RemoteLogger

/**
 * タスク#14(Androidプロセスkillからの黙示的セッション再アタッチ)の永続化レコード。
 *
 * `rust-core/src/reattach_persistence.rs`のモジュールdocに詳しい設計判断があるが要点だけ書くと:
 * SSHクライアント(russh)の暗号状態はプロセスのメモリ上にしか無くプロセスkillと共に消えるため、
 * `resume_client::SessionId`のワイヤーレベルRESUME(同一プロセス内でのネットワーク瞬断からの
 * 再接続に使う既存の機構)はプロセス再起動後には使えない(使おうとしても、鍵交換済みの
 * セッションの暗号化データ列の途中に新しいSSHハンドシェイクのバイト列が紛れ込むだけで、
 * ほぼ確実にリモートsshd側からMAC検証失敗等で即座に切断される)。
 *
 * そのため[reattachToken]はisekai-pipeのワイヤーレベルSessionIdそのものではなく、Kotlin側で
 * タブを開くたびに生成するローカルな記録識別子(診断ログ用)に過ぎない。実際の黙示的再接続は
 * 「このタブがどのプロファイルへの接続だったか」を覚えておき、プロセス再起動後に**通常の
 * 新規接続**(新しいSessionIdでの通常ATTACH)を自動的に(ユーザー操作無しで)やり直すことで
 * 実現する。サーバー側に残る古いparkセッションは`ISEKAI_PIPE_DESIGN.md` §8 Epic N-4の
 * `hello_with_parked_preemption`が新規ATTACH時に自動的に立ち退かせるため、この記録が
 * 明示的に何かを解放する必要は無い。
 *
 * @property tabId 記録時点での(旧プロセスの)タブID。復元後は新しいタブIDで開き直されるため、
 *   このIDそのものが復元後も使われ続けるわけではない(`ReattachStateStore`内でのレコード
 *   キーとしてのみ使う)。
 * @property profileId 接続先の[tools.isekai.terminal.data.ConnectionProfile.id]。
 * @property label 診断ログ用のプロファイルラベル(プロファイル自体が削除されていた場合の
 *   ログ表示にも使えるよう、参照ではなく値として保持する)。
 * @property reattachToken Kotlin側で生成したローカルな記録識別子。上記の通りワイヤー
 *   レベルSessionIdではない。
 * @property savedAtUnixSecs 記録時刻(Unix epoch秒)。タブを開いた時点、および接続が
 *   Connectedへ遷移するたびに更新される(「直近まで生きていたセッション」であることを
 *   なるべく正確に表すため)。
 */
data class ReattachRecord(
    val tabId: String,
    val profileId: Long,
    val label: String,
    val reattachToken: String,
    val savedAtUnixSecs: Long,
)

/**
 * [ReattachRecord]のリストを、Room ではなくプレーンなJSONファイルとして永続化する。
 *
 * Room(`AppDatabase.kt`)ではなくファイルベースを選んだ理由: このレコードは単純な
 * 「直近開いていたタブのリスト」であり、SQLクエリを要する複雑な検索・結合は発生しない
 * (JOIN・インデックス・部分更新クエリのいずれも不要)。CLAUDE.md の Room migration
 * ルール(`scripts/reserve-room-migration.sh`によるバージョン番号予約)はこの規模の
 * 永続化には見合わないコスト(複数の並行worktreeとの版数調整)であり、タスク#14の
 * 指示自体も「迷う場合はファイルベースを推奨」としている。
 *
 * ファイルI/Oはサスペンド関数化し、[mutex]で同時読み書きを直列化する(load-modify-save の
 * read-modify-write を複数コルーチンから同時に呼んでもレコードを取りこぼさないようにする、
 * `KeySequencePackInstallationRepository.installMutex`と同種の対処)。壊れた/存在しない
 * ファイルは空リストとして扱い、例外で呼び出し元を落とさない([load]参照)。
 */
class ReattachStateStore(private val file: File) {
    private val mutex = Mutex()

    /** 壊れたJSON・存在しないファイルは黙って空リストにフォールバックする
     *  (プロセスkill中の書き込み途中断・端末ストレージの問題等で壊れていても、
     *  アプリの起動自体をブロックしてはならないため)。 */
    suspend fun load(): List<ReattachRecord> = mutex.withLock { loadLocked() }

    private fun loadLocked(): List<ReattachRecord> {
        if (!file.exists()) return emptyList()
        val text = try {
            file.readText()
        } catch (e: Exception) {
            RemoteLogger.w("IsekaiTerminalReattach", "failed to read reattach state file: ${e.message}")
            return emptyList()
        }
        if (text.isBlank()) return emptyList()
        val arr = try {
            JSONArray(text)
        } catch (_: JSONException) {
            RemoteLogger.w("IsekaiTerminalReattach", "reattach state file contains invalid JSON, ignoring")
            return emptyList()
        }
        val result = mutableListOf<ReattachRecord>()
        for (i in 0 until arr.length()) {
            val o = arr.optJSONObject(i) ?: continue
            val record = decodeRecord(o) ?: continue
            result.add(record)
        }
        return result
    }

    private fun decodeRecord(o: JSONObject): ReattachRecord? {
        val tabId = o.optString("tabId", "")
        val profileId = o.optLong("profileId", -1L)
        val reattachToken = o.optString("reattachToken", "")
        val savedAtUnixSecs = o.optLong("savedAtUnixSecs", -1L)
        if (tabId.isEmpty() || profileId < 0 || reattachToken.isEmpty() || savedAtUnixSecs < 0) return null
        return ReattachRecord(
            tabId = tabId,
            profileId = profileId,
            label = o.optString("label", ""),
            reattachToken = reattachToken,
            savedAtUnixSecs = savedAtUnixSecs,
        )
    }

    private fun encodeRecord(record: ReattachRecord): JSONObject = JSONObject().apply {
        put("tabId", record.tabId)
        put("profileId", record.profileId)
        put("label", record.label)
        put("reattachToken", record.reattachToken)
        put("savedAtUnixSecs", record.savedAtUnixSecs)
    }

    /** [records]で全体を置き換える。一時ファイルへ書いてから`renameTo`することで、
     *  書き込み途中でプロセスがkillされても(このタスクがそもそも対象とするシナリオ)
     *  既存のファイルが壊れた内容で上書きされたままにはならないようにする。 */
    suspend fun save(records: List<ReattachRecord>) = mutex.withLock { saveLocked(records) }

    private fun saveLocked(records: List<ReattachRecord>) {
        val arr = JSONArray()
        for (record in records) arr.put(encodeRecord(record))
        try {
            val parent = file.parentFile
            if (parent != null && !parent.exists()) parent.mkdirs()
            val tmp = File(file.parentFile, "${file.name}.tmp")
            tmp.writeText(arr.toString())
            if (!tmp.renameTo(file)) {
                // renameTo は同一ファイルシステム間の移動のみ保証される。稀な失敗時は
                // 直接書き込みにフォールバックする(atomicityは失うが、書けないよりまし)。
                file.writeText(arr.toString())
                tmp.delete()
            }
        } catch (e: Exception) {
            RemoteLogger.w("IsekaiTerminalReattach", "failed to write reattach state file: ${e.message}")
        }
    }

    /** 同じ[ReattachRecord.tabId]の既存レコードを置き換える(無ければ追加する)。 */
    suspend fun upsert(record: ReattachRecord) = mutex.withLock {
        val current = loadLocked().filterNot { it.tabId == record.tabId }
        saveLocked(current + record)
    }

    /** [tabId]のレコードを削除する(存在しなければ何もしない)。 */
    suspend fun remove(tabId: String) = mutex.withLock {
        val current = loadLocked()
        val next = current.filterNot { it.tabId == tabId }
        if (next.size != current.size) saveLocked(next)
    }

    /** 全レコードを消す(復元処理の開始時、新しいタブIDで開き直す前に呼ぶ)。 */
    suspend fun clear() = mutex.withLock { saveLocked(emptyList()) }
}
