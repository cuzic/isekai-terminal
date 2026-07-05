package tools.isekai.terminal.input

import android.view.KeyEvent
import android.view.inputmethod.BaseInputConnection
import tools.isekai.terminal.util.RemoteLogger

class TerminalInputConnection(
    private val view: TerminalInputView,
) : BaseInputConnection(view, true) {

    override fun commitText(text: CharSequence?, newCursorPosition: Int): Boolean {
        val str = text?.toString() ?: return true
        if (view.ctrlArmed) {
            val codePoints = if (str.isEmpty()) 0 else str.codePointCount(0, str.length)
            val ctrlBytes = if (codePoints == 1) TerminalKeyEncoder.ctrlByte(str.codePointAt(0)) else null
            // 変換の成否に関わらずここで武装状態を消費する（押しっぱなし化防止）。
            // Compose 側の状態も onCtrlConsumed() 経由で必ず OFF に戻す。
            view.ctrlArmed = false
            view.onCtrlConsumed?.invoke()
            if (ctrlBytes != null) {
                view.onComposingText?.invoke("")
                view.onSendBytes?.invoke(ctrlBytes)
                return true
            }
            // 変換不可（日本語確定等）: 通常のコミット処理にフォールスルーする
        }
        view.onComposingText?.invoke("")
        if (str.isNotEmpty()) {
            val codePoints = str.codePointCount(0, str.length)
            if (codePoints > 1 && view.bracketedPasteMode)
                RemoteLogger.i("IsekaiTerminalIME", "paste $codePoints codepoints → bracketed paste")
            view.onSendBytes?.invoke(TerminalKeyEncoder.commitTextBytes(str, view.bracketedPasteMode))
        }
        return super.commitText(text, newCursorPosition)
    }

    override fun setComposingText(text: CharSequence?, newCursorPosition: Int): Boolean {
        val newText = text?.toString() ?: ""
        RemoteLogger.d("IsekaiTerminalIME", "composing: '${newText.take(10)}'")
        view.onComposingText?.invoke(newText)
        return super.setComposingText(text, newCursorPosition)
    }

    override fun finishComposingText(): Boolean {
        val pending = composingText()
        view.onComposingText?.invoke("")
        if (pending.isNotEmpty()) {
            RemoteLogger.d("IsekaiTerminalIME", "composing finish: '${pending.take(20)}' (${pending.length} chars) → sent")
            view.onSendBytes?.invoke(pending.toByteArray(Charsets.UTF_8))
        }
        return super.finishComposingText()
    }

    override fun deleteSurroundingText(beforeLength: Int, afterLength: Int): Boolean {
        val current = composingText()
        if (current.isNotEmpty()) {
            val newText = current.dropLast(beforeLength.coerceAtMost(current.length))
            setComposingText(newText, 1)
            return true
        }
        repeat(beforeLength) { view.onSendBytes?.invoke(byteArrayOf(0x7F)) }
        return true
    }

    override fun sendKeyEvent(event: KeyEvent): Boolean {
        if (event.action == KeyEvent.ACTION_DOWN) {
            TerminalKeyEncoder.specialKeyBytes(event.keyCode, view.applicationCursorMode)?.let {
                view.onSendBytes?.invoke(it)
                return true
            }
            // 物理 Ctrl 併用時はトグルを消費せず素通し（二重変換防止）
            if (view.ctrlArmed && !event.isCtrlPressed) {
                val ctrlBytes = TerminalKeyEncoder.ctrlByte(event.getUnicodeChar(0))
                view.ctrlArmed = false
                view.onCtrlConsumed?.invoke()
                if (ctrlBytes != null) {
                    view.onSendBytes?.invoke(ctrlBytes)
                    return true
                }
                // 変換不可: 通常のキー処理にフォールスルーする
            }
            TerminalKeyEncoder.unicodeCharBytes(event.getUnicodeChar(event.metaState))?.let {
                view.onSendBytes?.invoke(it)
                return true
            }
        }
        return super.sendKeyEvent(event)
    }

    override fun getTextBeforeCursor(n: Int, flags: Int): CharSequence = ""

    private fun composingText(): String {
        val editable = getEditable() ?: return ""
        val start = getComposingSpanStart(editable)
        val end = getComposingSpanEnd(editable)
        if (start < 0 || end < 0 || start > end) return ""
        return editable.toString().substring(start, end)
    }
}
