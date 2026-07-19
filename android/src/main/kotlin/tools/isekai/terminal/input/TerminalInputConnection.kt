package tools.isekai.terminal.input

import android.view.KeyEvent
import android.view.inputmethod.BaseInputConnection
import tools.isekai.terminal.util.RemoteLogger
import uniffi.isekai_terminal_core.TerminalKeyModifiers

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
            // IME 変換中（日本語 henkan 中など、確定前の composing テキストが残っている間）は
            // 以下の物理ショートカット判定（アプリレベルショートカット・Ctrl/Alt 明示送出）を
            // すべてスキップし、通常のキー入力／IME への委譲に任せる。変換中に Ctrl+<key> 等を
            // ターミナル制御コードとして送ってしまうと変換中の文字列や候補が壊れるため
            // （日本語 IME 完全対応はこのプロジェクトの差別化ポイント、絶対に壊さない）。
            val composing = composingText().isNotEmpty()

            if (!composing && handleShortcut(event)) return true

            // 物理修飾キー(Shift/Alt/Ctrl/Meta)の現在状態。矢印・Home/End・PageUp/Down・F1〜F12に
            // xterm互換の修飾子付きシーケンス(`ESC[1;5A`等)を反映するため、specialKeyBytesへ
            // そのまま渡す(rust-core`terminal_special_key_bytes`(タスク#29)と同一golden表)。
            // ソフトキーボードのトグル式Ctrl(view.ctrlArmed)とは独立で、実キーボードの修飾キーのみを見る。
            //
            // NumLock(event.isNumLockOn)がOFFの外付けキーボードでは、テンキーの数字キーは
            // 矢印/Home/End/PageUp/Down相当のナビゲーションクラスタとして扱われるのが実キーボードの
            // 標準的な挙動。この判定はKeyEvent(物理イベント)を必要とするため、KeyEventを持たない
            // 純粋関数のTerminalKeyEncoderではなくここで行う(タスク#83、codexレビュー指摘)。
            val modifiers = TerminalKeyModifiers(
                shift = event.isShiftPressed,
                alt = event.isAltPressed,
                ctrl = event.isCtrlPressed,
                meta = event.isMetaPressed,
            )
            val effectiveKeyCode = numpadKeyCodeRespectingNumLock(event.keyCode, event.isNumLockOn)
            TerminalKeyEncoder.specialKeyBytes(effectiveKeyCode, view.applicationCursorMode, view.applicationKeypadMode, modifiers, view.kittyKeyboardFlags)?.let {
                view.onSendBytes?.invoke(it)
                return true
            }
            // JIS配列固有キー(¥/ろ)。これらのキーコードはJIS配列の物理キーボードでしか
            // 生成され得ないため、US配列キーボードの通常入力を誤って横取りすることはない。
            if (KeyboardLayoutDetector.resolveJisLayout(view.keyboardLayoutMode, event.device)) {
                TerminalKeyEncoder.jisSpecialKeyBytes(event.keyCode, event.isShiftPressed)?.let {
                    view.onSendBytes?.invoke(it)
                    return true
                }
            }

            // Kitty keyboard protocol(タスク#54)のdisambiguate escape codes(bit0)が交渉
            // されている場合、Ctrl/Alt(併用含む)付きの印字可能文字キーはCSI u形式で送る
            // (タスク#91、Kitty仕様がEnter/Tab/Backspace以外の修飾キー付き印字可能文字を
            // 対象にするため。未交渉時はnullを返しlegacyエンコードへフォールスルーする)。
            // IME変換中は他の物理修飾キー分岐と同様に誤発火防止のため無効。
            if (!composing && (event.isCtrlPressed || event.isAltPressed)) {
                TerminalKeyEncoder.kittyDisambiguatedKeyBytes(event.getUnicodeChar(0), modifiers, view.kittyKeyboardFlags)?.let {
                    view.onSendBytes?.invoke(it)
                    return true
                }
            }

            // 物理 Ctrl 押下（トグルではなく実キーボードの修飾キー）: Ctrl+A〜Z 等を制御コードと
            // して明示的に送出する。Alt 併用時は下の Alt 分岐に譲る。IME 変換中は誤発火防止のため無効。
            if (!composing && event.isCtrlPressed && !event.isAltPressed) {
                TerminalKeyEncoder.ctrlByte(event.getUnicodeChar(0))?.let {
                    view.onSendBytes?.invoke(it)
                    return true
                }
            }

            // 物理 Alt 押下: xterm の "meta sends escape" 相当（ESC プレフィックス）。
            // IME 変換中は誤発火防止のため無効。
            if (!composing && event.isAltPressed && !event.isCtrlPressed) {
                TerminalKeyEncoder.altKeyBytes(event.getUnicodeChar(0))?.let {
                    view.onSendBytes?.invoke(it)
                    return true
                }
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

    /**
     * アプリレベルの物理キーボードショートカット（コピー／貼り付け／タブ切替）を判定して
     * 対応する [view] のコールバックを呼ぶ。コールバックが設定されていない（呼び出し元が
     * 配線していない）場合は何もせず false を返し、呼び出し元が通常のキー処理へ
     * フォールスルーできるようにする（例: タブ機能が無い文脈で Ctrl+Tab を押しても
     * 素の Tab 送出にフォールバックする）。
     */
    private fun handleShortcut(event: KeyEvent): Boolean {
        val ctrl = event.isCtrlPressed
        val shift = event.isShiftPressed
        val meta = event.isMetaPressed
        return when {
            ctrl && !shift && event.keyCode == KeyEvent.KEYCODE_TAB -> invokeShortcut(view.onNextTabRequested)
            ctrl && shift && event.keyCode == KeyEvent.KEYCODE_TAB -> invokeShortcut(view.onPreviousTabRequested)
            event.keyCode == KeyEvent.KEYCODE_COPY -> invokeShortcut(view.onCopyRequested)
            event.keyCode == KeyEvent.KEYCODE_PASTE -> invokeShortcut(view.onPasteRequested)
            (ctrl && shift && event.keyCode == KeyEvent.KEYCODE_C) || (meta && event.keyCode == KeyEvent.KEYCODE_C) ->
                invokeShortcut(view.onCopyRequested)
            (ctrl && shift && event.keyCode == KeyEvent.KEYCODE_V) || (meta && event.keyCode == KeyEvent.KEYCODE_V) ->
                invokeShortcut(view.onPasteRequested)
            else -> false
        }
    }

    /**
     * NumLockがOFFの物理外付けキーボードでのテンキー数字キーを、対応するナビゲーション
     * クラスタのキーコードに置き換える(タスク#83)。NumLock ONの場合、またはナビゲーション
     * 相当が無いキー(中央の5・四則演算子・Enter)はそのまま返す(四則演算子/EnterはNumLockの
     * 影響を受けないのが実キーボードの慣習)。`0`→Insert、小数点→前方Delete
     * (`KC_INSERT`/`KC_FORWARD_DEL`、`ESC[2~`/`ESC[3~`、rust-coreの`TerminalSpecialKey::ForwardDelete`
     * と同一シーケンス)。
     *
     * [TerminalKeyEncoder]自体は物理[KeyEvent]を持たない純粋関数として保つため、この判定は
     * KeyEventを保持している呼び出し元([sendKeyEvent])側で行う。マクロ/打鍵列経由の仮想キー
     * 送信([tools.isekai.terminal.KeySequenceCommands])は物理KeyEventを経由せず
     * `TerminalKeyEncoder.specialKeyBytes`を直接呼ぶため、この変換の影響を受けない。
     */
    private fun numpadKeyCodeRespectingNumLock(keyCode: Int, numLockOn: Boolean): Int {
        if (numLockOn) return keyCode
        return when (keyCode) {
            TerminalKeyEncoder.KC_NUMPAD_7 -> TerminalKeyEncoder.KC_MOVE_HOME
            TerminalKeyEncoder.KC_NUMPAD_8 -> TerminalKeyEncoder.KC_DPAD_UP
            TerminalKeyEncoder.KC_NUMPAD_9 -> TerminalKeyEncoder.KC_PAGE_UP
            TerminalKeyEncoder.KC_NUMPAD_4 -> TerminalKeyEncoder.KC_DPAD_LEFT
            TerminalKeyEncoder.KC_NUMPAD_6 -> TerminalKeyEncoder.KC_DPAD_RIGHT
            TerminalKeyEncoder.KC_NUMPAD_1 -> TerminalKeyEncoder.KC_MOVE_END
            TerminalKeyEncoder.KC_NUMPAD_2 -> TerminalKeyEncoder.KC_DPAD_DOWN
            TerminalKeyEncoder.KC_NUMPAD_3 -> TerminalKeyEncoder.KC_PAGE_DOWN
            TerminalKeyEncoder.KC_NUMPAD_0   -> TerminalKeyEncoder.KC_INSERT
            TerminalKeyEncoder.KC_NUMPAD_DOT -> TerminalKeyEncoder.KC_FORWARD_DEL
            else -> keyCode
        }
    }

    private fun invokeShortcut(callback: (() -> Unit)?): Boolean {
        callback ?: return false
        callback.invoke()
        return true
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
