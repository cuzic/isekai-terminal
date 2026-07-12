package tools.isekai.terminal.input

import android.content.Context
import android.graphics.Rect
import android.text.InputType
import android.util.AttributeSet
import android.view.KeyEvent
import android.view.View
import android.view.inputmethod.EditorInfo
import android.view.inputmethod.InputConnection
import tools.isekai.terminal.util.RemoteLogger

class TerminalInputView @JvmOverloads constructor(
    context: Context,
    attrs: AttributeSet? = null,
) : View(context, attrs) {

    var onSendBytes: ((ByteArray) -> Unit)? = null

    var onComposingText: ((String) -> Unit)? = null

    var applicationCursorMode: Boolean = false
    var bracketedPasteMode: Boolean = false

    /**
     * トグル式 Ctrl キーの武装状態。true の間に次に入力された 1 文字を
     * Ctrl+<key> の制御コードに変換して送信する（表示専用の UI ローカル状態）。
     */
    var ctrlArmed: Boolean = false

    /** Ctrl トグルが 1 文字を消費した（変換の成否に関わらず）ときに呼ばれる。UI 側で OFF 表示に戻す用。 */
    var onCtrlConsumed: (() -> Unit)? = null

    // ── 物理キーボードのアプリレベルショートカット ──────────────────────
    // 対応するコールバックが null の間はショートカットとして扱わず、通常のキー処理へ
    // フォールスルーする（[TerminalInputConnection.handleShortcut] 参照）。
    // いずれも IME 変換中（composing テキストが残っている間）は
    // [TerminalInputConnection.sendKeyEvent] 側で呼び出し自体をスキップする
    // （日本語 IME 変換の誤中断防止。差別化ポイントである日本語 IME 対応を壊さないための措置）。

    /** コピー: 物理 Ctrl+Shift+C / Meta(Cmd)+C / ハードウェア Copy キー。 */
    var onCopyRequested: (() -> Unit)? = null

    /** 貼り付け: 物理 Ctrl+Shift+V / Meta(Cmd)+V / ハードウェア Paste キー。 */
    var onPasteRequested: (() -> Unit)? = null

    /** 次のタブへ切り替え: 物理 Ctrl+Tab。 */
    var onNextTabRequested: (() -> Unit)? = null

    /** 前のタブへ切り替え: 物理 Ctrl+Shift+Tab。 */
    var onPreviousTabRequested: (() -> Unit)? = null

    init {
        isFocusable = true
        isFocusableInTouchMode = true
    }

    private var currentConnection: TerminalInputConnection? = null

    override fun onCreateInputConnection(outAttrs: EditorInfo): InputConnection {
        outAttrs.inputType = InputType.TYPE_CLASS_TEXT or
                InputType.TYPE_TEXT_FLAG_NO_SUGGESTIONS
        outAttrs.imeOptions = EditorInfo.IME_FLAG_NO_FULLSCREEN or
                EditorInfo.IME_FLAG_NO_EXTRACT_UI
        return TerminalInputConnection(this).also { currentConnection = it }
    }

    fun commitComposing() {
        currentConnection?.finishComposingText()
    }

    override fun onCheckIsTextEditor(): Boolean = true

    override fun onFocusChanged(gainFocus: Boolean, direction: Int, previouslyFocusedRect: Rect?) {
        super.onFocusChanged(gainFocus, direction, previouslyFocusedRect)
        RemoteLogger.i("IsekaiTerminalIME", "input view focus: $gainFocus (onSendBytes=${onSendBytes != null})")
    }

    override fun onKeyDown(keyCode: Int, event: KeyEvent): Boolean {
        if (currentConnection?.sendKeyEvent(event) == true) return true
        return super.onKeyDown(keyCode, event)
    }
}
