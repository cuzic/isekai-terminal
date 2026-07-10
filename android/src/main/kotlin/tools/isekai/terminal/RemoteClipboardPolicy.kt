package tools.isekai.terminal

/**
 * リモート(ホスト側)からのクリップボード書き込み/読み出し要求を、opt-in設定
 * ([PREF_KEY_ALLOW_REMOTE_CLIPBOARD_WRITE]/[PREF_KEY_ALLOW_REMOTE_CLIPBOARD_PULL]、
 * 既定OFF)でゲートするだけの純粋なロジック。SharedPreferences/ClipboardManagerの
 * 実アクセスは呼び出し元(`TerminalTabsViewModel`の本番用コンストラクタ)が
 * ラムダとして注入するため、このクラス自体はAndroidフレームワークに依存せず
 * 素のJVMユニットテストで検証できる([TerminalSession]へ渡す
 * `onClipboardWriteRequested`/`onClipboardPullRequested`が本番用コンストラクタの
 * ラムダに直書きされていてテストから到達できなかった問題への対応)。
 */
class RemoteClipboardPolicy(
    private val isWriteAllowed: () -> Boolean,
    private val isPullAllowed: () -> Boolean,
    private val writeToClipboard: (String) -> Unit,
    private val readFromClipboard: () -> String?,
) {
    fun onClipboardWriteRequested(text: String) {
        if (isWriteAllowed()) writeToClipboard(text)
    }

    fun onClipboardPullRequested(): String? =
        if (isPullAllowed()) readFromClipboard() else null
}
