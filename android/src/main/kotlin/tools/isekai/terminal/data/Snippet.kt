package tools.isekai.terminal.data

import android.os.Parcelable
import androidx.room.*
import kotlinx.parcelize.Parcelize

/**
 * 定型コマンド（スニペット）。[profileId] が null なら全プロファイル共通、非null なら
 * その ID のプロファイル専用として表示される。
 */
@Parcelize
@Entity(tableName = "snippets")
data class Snippet(
    @PrimaryKey(autoGenerate = true) val id: Long = 0,
    val label: String,
    val command: String,
    @ColumnInfo(name = "sort_order") val sortOrder: Int = 0,
    @ColumnInfo(name = "profile_id") val profileId: Long? = null,
    @ColumnInfo(name = "append_newline") val appendNewline: Boolean = true,
) : Parcelable

@Dao
interface SnippetDao {
    @Query("SELECT * FROM snippets ORDER BY sort_order ASC, label ASC")
    suspend fun getAll(): List<Snippet>

    @Query(
        "SELECT * FROM snippets WHERE profile_id IS NULL OR profile_id = :profileId " +
            "ORDER BY sort_order ASC, label ASC"
    )
    suspend fun getForProfile(profileId: Long): List<Snippet>

    @Insert(onConflict = OnConflictStrategy.REPLACE)
    suspend fun upsert(snippet: Snippet): Long

    @Delete
    suspend fun delete(snippet: Snippet)

    @Query("SELECT * FROM snippets WHERE id = :id LIMIT 1")
    suspend fun findById(id: Long): Snippet?
}
