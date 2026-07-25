package tools.isekai.terminal.data

import androidx.room.ColumnInfo
import androidx.room.Dao
import androidx.room.Entity
import androidx.room.Insert
import androidx.room.OnConflictStrategy
import androidx.room.PrimaryKey
import androidx.room.Query

/**
 * タスク#60: プロファイル(の primary pane)が使っている tmux session group の
 * ウィンドウを長期的に指すタグの永続化。
 *
 * `TerminalTabsViewModel.TabState.tabId`はプロセス内限定の`UUID.randomUUID()`で
 * アプリ再起動を跨いで復元されない(タブ一覧自体を永続化・復元する機能が現状
 * 存在しない)ため、永続化キーには代わりに[ConnectionProfile.id](安定)を使う——
 * 「このプロファイルを開いたら、前回と同じtmuxウィンドウに戻る」という
 * プロファイル単位の粒度になる(タブ単位ではない点に注意)。
 *
 * split pane側は対象外(primary paneのみtmuxへマッピングするMVP判断、
 * `rust-core/src/tmux_session.rs`のモジュールdoc参照)。
 *
 * `tag`だけを永続化し、tmuxウィンドウインデックス等は保持しない
 * (`TmuxCoordinates`同様、揮発性の値を永続化キーにしないという方針を
 * Kotlin側でも踏襲する)。
 */
@Entity(tableName = "tmux_tab_locators")
data class TmuxTabLocator(
    @PrimaryKey @ColumnInfo(name = "profile_id") val profileId: Long,
    val tag: String,
    @ColumnInfo(name = "updated_at") val updatedAt: Long = System.currentTimeMillis(),
)

@Dao
interface TmuxTabLocatorDao {
    @Query("SELECT * FROM tmux_tab_locators WHERE profile_id = :profileId LIMIT 1")
    suspend fun findByProfileId(profileId: Long): TmuxTabLocator?

    @Insert(onConflict = OnConflictStrategy.REPLACE)
    suspend fun upsert(locator: TmuxTabLocator)

    @Query("DELETE FROM tmux_tab_locators WHERE profile_id = :profileId")
    suspend fun deleteByProfileId(profileId: Long)
}
