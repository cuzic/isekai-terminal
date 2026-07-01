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
        RemoteLogger.i("TsshIME", "input view focus: $gainFocus (onSendBytes=${onSendBytes != null})")
    }

    override fun onKeyDown(keyCode: Int, event: KeyEvent): Boolean {
        if (currentConnection?.sendKeyEvent(event) == true) return true
        return super.onKeyDown(keyCode, event)
    }
}
