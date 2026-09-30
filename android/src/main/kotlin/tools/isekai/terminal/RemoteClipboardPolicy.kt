package tools.isekai.terminal

import tools.isekai.terminal.util.RemoteLogger
import uniffi.isekai_terminal_core.ClipboardPayload

/**
 * リモート(ホスト側)からのクリップボード書き込み/読み出し要求を、opt-in設定
 * ([PREF_KEY_ALLOW_REMOTE_CLIPBOARD_WRITE]/[PREF_KEY_ALLOW_REMOTE_CLIPBOARD_PULL]、
 * 既定OFF)でゲートするだけの純粋なロジック。SharedPreferences/ClipboardManagerの
 * 実アクセスは呼び出し元(`TerminalTabsViewModel`の本番用コンストラクタ)が
 * ラムダとして注入するため、このクラス自体はAndroidフレームワークに依存せず
 * 素のJVMユニットテストで検証できる([TerminalSession]へ渡す
 * `onClipboardWriteRequested`/`onClipboardPullRequested`が本番用コンストラクタの
 * ラムダに直書きされていてテストから到達できなかった問題への対応)。
 * `text: String`専用だった旧シグネチャは[ClipboardPayload](mime + `ByteArray`)へ
 * 置き換えた——テキスト/画像どちらのmimeが来るかの判断はRust側(`session.rs`)が
 * 既に行っているので、ここでは単にopt-inチェックを通すだけでよい。
 */
class RemoteClipboardPolicy(
    private val isWriteAllowed: () -> Boolean,
    private val isPullAllowed: () -> Boolean,
    private val writeToClipboard: (ClipboardPayload) -> Unit,
    private val readFromClipboard: () -> ClipboardPayload?,
) {
    // AND-L1: どちらもRust側スレッドからUniFFI callback境界越しに呼ばれる。ここで例外
    // (他アプリのURI権限切れによるSecurityException等)やOOM(巨大画像のデコード)が
    // 漏れるとcallback境界で「想定外エラー」になるため、捕捉して「何もしない/応答なし」に落とす。

    fun onClipboardWriteRequested(payload: ClipboardPayload) {
        try {
            if (isWriteAllowed()) writeToClipboard(payload)
        } catch (e: Exception) {
            RemoteLogger.w(TAG, "clipboard write failed (ignored): ${e.javaClass.simpleName}: ${e.message}")
        } catch (e: OutOfMemoryError) {
            RemoteLogger.w(TAG, "clipboard write ran out of memory (ignored)")
        }
    }

    fun onClipboardPullRequested(): ClipboardPayload? =
        try {
            if (isPullAllowed()) readFromClipboard() else null
        } catch (e: Exception) {
            RemoteLogger.w(TAG, "clipboard pull failed (no reply): ${e.javaClass.simpleName}: ${e.message}")
            null
        } catch (e: OutOfMemoryError) {
            RemoteLogger.w(TAG, "clipboard pull ran out of memory (no reply)")
            null
        }

    private companion object {
        const val TAG = "IsekaiTerminalClipboard"
    }
}
