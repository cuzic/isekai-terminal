package tools.isekai.terminal.util

import android.content.Context
import java.util.UUID

/**
 * タスク#60: このアプリインストール固有の、永続的で一意な識別トークン。
 *
 * tmux session group(`rust-core/src/tmux_session.rs`)は「同じデバイスからの
 * 再接続は常に同じグループメンバー(セッション名)に戻り、別デバイスは別の
 * グループメンバーになる」という前提で設計されている。この関数が返す値は
 * その`client_id`としてRust側(`SessionOrchestrator.ensureTmuxTabWindow`)へ
 * そのまま渡すだけの不透明なトークンであり、Kotlin側で何かを判断する材料には
 * しない(命名規則の決定・ハッシュ化は全てRust側、`.claude/rules/rust-ssot.md`)。
 *
 * 端末固有のANDROID_ID等ではなくアプリ生成のランダムUUIDを使う: アプリを
 * アンインストール/再インストールすれば新しい値になる(=前回とは別グループ
 * メンバー扱いになる)方が、ANDROID_IDの取得可否・プライバシー上の懸念に
 * 左右されない安定した実装になるため。
 */
object ClientIdentity {
    private const val PREFS_NAME = "isekai_terminal_ui"
    private const val KEY_CLIENT_ID = "tmux_client_id"

    /**
     * read-then-writeを`synchronized`で直列化し、書き込みも`commit()`(同期)にする
     * (opusレビューLow指摘: 複数タブがほぼ同時に`getOrCreate`を呼ぶと、両方が
     * `getString`でnullを見て別々のUUIDを生成・`apply()`(非同期)してしまい、
     * 「同じデバイスからの再接続は常に同じtmux session group」という前提が
     * 崩れ得た)。呼び出し元は`viewModelScope.launch(ioDispatcher)`内で呼ぶため、
     * `commit()`の同期I/Oはブロッキングの懸念にならない。
     */
    @Synchronized
    fun getOrCreate(context: Context): String {
        val prefs = context.getSharedPreferences(PREFS_NAME, Context.MODE_PRIVATE)
        return prefs.getString(KEY_CLIENT_ID, null) ?: UUID.randomUUID().toString().also { fresh ->
            prefs.edit().putString(KEY_CLIENT_ID, fresh).commit()
        }
    }
}
