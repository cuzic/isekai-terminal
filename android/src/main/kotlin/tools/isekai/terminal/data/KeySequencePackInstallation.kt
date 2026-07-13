package tools.isekai.terminal.data

import androidx.room.*
import tools.isekai.terminal.input.KeyStep
import tools.isekai.terminal.pack.PackParamValuesJson

/**
 * 打鍵列セット(パック)の有効化状態。パック定義自体([tools.isekai.terminal.pack.KeySequencePack])は
 * DB行ではなくアプリ同梱の静的データであり、この行は「どのパックを、どのパラメータ値で
 * 有効化しているか」だけを持つ(ライブバインディング方式、#18タスク参照)。
 *
 * `profileId` が null ならグローバル有効化。グローバル有効化は同一`packId`につき常に高々1行に
 * なるようアプリ側([KeySequencePackInstallationRepository])で保証する(SQLiteのUNIQUE制約は
 * NULL列を重複除外の対象外にするため、DB制約には頼らない)。
 */
@Entity(tableName = "key_sequence_pack_installations")
data class KeySequencePackInstallation(
    @PrimaryKey(autoGenerate = true) val id: Long = 0,
    @ColumnInfo(name = "pack_id") val packId: String,
    val version: Int,
    @ColumnInfo(name = "param_values_json") val paramValuesJson: String,
    @ColumnInfo(name = "profile_id") val profileId: Long? = null,
) {
    val paramValues: Map<String, KeyStep> get() = PackParamValuesJson.decode(paramValuesJson)

    companion object {
        fun create(packId: String, version: Int, paramValues: Map<String, KeyStep>, profileId: Long? = null, id: Long = 0) =
            KeySequencePackInstallation(
                id = id,
                packId = packId,
                version = version,
                paramValuesJson = PackParamValuesJson.encode(paramValues),
                profileId = profileId,
            )
    }
}

@Dao
interface KeySequencePackInstallationDao {
    @Query("SELECT * FROM key_sequence_pack_installations")
    suspend fun getAll(): List<KeySequencePackInstallation>

    @Query("SELECT * FROM key_sequence_pack_installations WHERE pack_id = :packId AND profile_id IS NULL LIMIT 1")
    suspend fun findGlobal(packId: String): KeySequencePackInstallation?

    @Query("SELECT * FROM key_sequence_pack_installations WHERE pack_id = :packId AND profile_id = :profileId LIMIT 1")
    suspend fun findForProfile(packId: String, profileId: Long): KeySequencePackInstallation?

    @Insert(onConflict = OnConflictStrategy.REPLACE)
    suspend fun upsert(installation: KeySequencePackInstallation): Long

    @Delete
    suspend fun delete(installation: KeySequencePackInstallation)
}
