package com.example.imespike.input

/**
 * キーコード→バイト列の変換ロジック。
 * Android 依存なし（定数は android.view.KeyEvent と一致させている）。
 */
object TerminalKeyEncoder {

    // android.view.KeyEvent 定数と同値（変更不可の Android API 値）
    const val KC_DPAD_UP    = 19
    const val KC_DPAD_DOWN  = 20
    const val KC_DPAD_LEFT  = 21
    const val KC_DPAD_RIGHT = 22
    const val KC_ENTER      = 66
    const val KC_DEL        = 67
    const val KC_TAB        = 61
    const val KC_ESCAPE     = 111
    const val KC_PAGE_UP    = 92
    const val KC_PAGE_DOWN  = 93
    const val KC_MOVE_HOME  = 122
    const val KC_MOVE_END   = 123

    /**
     * 特殊キーのバイト列。未定義なら null。
     * applicationCursorMode=true のとき矢印キーは SS3 シーケンス（vim 等で必要）。
     */
    fun specialKeyBytes(keyCode: Int, applicationCursorMode: Boolean = false): ByteArray? = when (keyCode) {
        KC_ENTER      -> byteArrayOf(0x0D)
        KC_DEL        -> byteArrayOf(0x7F)
        KC_TAB        -> byteArrayOf(0x09)
        KC_ESCAPE     -> byteArrayOf(0x1B)
        KC_DPAD_UP    -> if (applicationCursorMode) byteArrayOf(0x1B, 0x4F, 0x41) else byteArrayOf(0x1B, 0x5B, 0x41)
        KC_DPAD_DOWN  -> if (applicationCursorMode) byteArrayOf(0x1B, 0x4F, 0x42) else byteArrayOf(0x1B, 0x5B, 0x42)
        KC_DPAD_RIGHT -> if (applicationCursorMode) byteArrayOf(0x1B, 0x4F, 0x43) else byteArrayOf(0x1B, 0x5B, 0x43)
        KC_DPAD_LEFT  -> if (applicationCursorMode) byteArrayOf(0x1B, 0x4F, 0x44) else byteArrayOf(0x1B, 0x5B, 0x44)
        KC_PAGE_UP    -> byteArrayOf(0x1B, 0x5B, 0x35, 0x7E)
        KC_PAGE_DOWN  -> byteArrayOf(0x1B, 0x5B, 0x36, 0x7E)
        KC_MOVE_HOME  -> byteArrayOf(0x1B, 0x5B, 0x48)
        KC_MOVE_END   -> byteArrayOf(0x1B, 0x5B, 0x46)
        else          -> null
    }

    /** Unicode コードポイント→バイト列。0 なら null。 */
    fun unicodeCharBytes(unicodeChar: Int): ByteArray? {
        if (unicodeChar == 0) return null
        return if (unicodeChar < 0x20 || unicodeChar == 0x7F) {
            byteArrayOf(unicodeChar.toByte())
        } else {
            unicodeChar.toChar().toString().toByteArray(Charsets.UTF_8)
        }
    }

    /**
     * IME 確定テキスト→バイト列。
     * 複数コードポイントかつ bracketedPasteMode が有効な場合のみブラケットペーストで囲む。
     * サロゲートペア（絵文字等）を正しく 1 コードポイントとして扱うため codePointCount を使用。
     */
    fun commitTextBytes(text: String, bracketedPasteMode: Boolean = false): ByteArray {
        if (text.isEmpty()) return ByteArray(0)
        val codePoints = text.codePointCount(0, text.length)
        return if (codePoints > 1 && bracketedPasteMode) {
            byteArrayOf(0x1B, 0x5B, 0x32, 0x30, 0x30, 0x7E) +  // ESC[200~
            text.toByteArray(Charsets.UTF_8) +
            byteArrayOf(0x1B, 0x5B, 0x32, 0x30, 0x31, 0x7E)    // ESC[201~
        } else {
            text.toByteArray(Charsets.UTF_8)
        }
    }
}
