package tools.isekai.terminal.data

import android.os.Parcelable
import androidx.room.*
import kotlinx.parcelize.Parcelize
import tools.isekai.terminal.input.KeyStep
import tools.isekai.terminal.input.KeyStepJson

/**
 * ユーザー定義の打鍵列(KeySequence)。[profileId] が null なら全プロファイル共通、
 * 非null ならその ID のプロファイル専用として表示される([Snippet] と同じ運用)。
 *
 * `sourcePackId` 列は持たない: 打鍵列セット(パック、`KeySequencePack`)はライブバインディング方式で
 * DB行へマテリアライズしないため、「パック由来かどうか」をこのテーブル側で表現する必要がない。
 */
@Parcelize
@Entity(tableName = "key_sequences")
data class KeySequence(
    @PrimaryKey(autoGenerate = true) val id: Long = 0,
    val label: String,
    @ColumnInfo(name = "steps_json") val stepsJson: String,
    @ColumnInfo(name = "sort_order") val sortOrder: Int = 0,
    @ColumnInfo(name = "profile_id") val profileId: Long? = null,
) : Parcelable {
    /** [stepsJson] を復元した [KeyStep] のリスト。壊れたJSONは空リストにフォールバックする。 */
    val steps: List<KeyStep> get() = KeyStepJson.decode(stepsJson)

    companion object {
        fun create(label: String, steps: List<KeyStep>, sortOrder: Int = 0, profileId: Long? = null, id: Long = 0) =
            KeySequence(id = id, label = label, stepsJson = KeyStepJson.encode(steps), sortOrder = sortOrder, profileId = profileId)
    }
}

@Dao
interface KeySequenceDao {
    @Query("SELECT * FROM key_sequences ORDER BY sort_order ASC, label ASC")
    suspend fun getAll(): List<KeySequence>

    @Query(
        "SELECT * FROM key_sequences WHERE profile_id IS NULL OR profile_id = :profileId " +
            "ORDER BY sort_order ASC, label ASC"
    )
    suspend fun getForProfile(profileId: Long): List<KeySequence>

    @Insert(onConflict = OnConflictStrategy.REPLACE)
    suspend fun upsert(keySequence: KeySequence): Long

    @Delete
    suspend fun delete(keySequence: KeySequence)

    @Query("SELECT * FROM key_sequences WHERE id = :id LIMIT 1")
    suspend fun findById(id: Long): KeySequence?
}
