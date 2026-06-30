package com.example.imespike.input

import android.view.KeyEvent
import android.view.inputmethod.BaseInputConnection
import com.example.imespike.util.RemoteLogger

class TerminalInputConnection(
    private val view: TerminalInputView,
) : BaseInputConnection(view, true) {

    override fun commitText(text: CharSequence?, newCursorPosition: Int): Boolean {
        val str = text?.toString() ?: return true
        view.onComposingText?.invoke("")
        if (str.isNotEmpty()) {
            val codePoints = str.codePointCount(0, str.length)
            if (codePoints > 1 && view.bracketedPasteMode)
                RemoteLogger.i("TsshIME", "paste $codePoints codepoints → bracketed paste")
            view.onSendBytes?.invoke(TerminalKeyEncoder.commitTextBytes(str, view.bracketedPasteMode))
        }
        return super.commitText(text, newCursorPosition)
    }

    override fun setComposingText(text: CharSequence?, newCursorPosition: Int): Boolean {
        val newText = text?.toString() ?: ""
        RemoteLogger.d("TsshIME", "composing: '${newText.take(10)}'")
        view.onComposingText?.invoke(newText)
        return super.setComposingText(text, newCursorPosition)
    }

    override fun finishComposingText(): Boolean {
        val pending = composingText()
        view.onComposingText?.invoke("")
        if (pending.isNotEmpty()) {
            RemoteLogger.d("TsshIME", "composing finish: '${pending.take(20)}' (${pending.length} chars) → sent")
            view.onSendBytes?.invoke(pending.toByteArray(Charsets.UTF_8))
        }
        return super.finishComposingText()
    }

    override fun deleteSurroundingText(beforeLength: Int, afterLength: Int): Boolean {
        if (composingText().isNotEmpty()) {
            val result = super.deleteSurroundingText(beforeLength, afterLength)
            view.onComposingText?.invoke(composingText())
            return result
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
