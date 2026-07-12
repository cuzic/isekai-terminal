package tools.isekai.terminal.input

/**
 * キーコード→バイト列の変換ロジック。
 * Android 依存なし（定数は android.view.KeyEvent と一致させている）。
 *
 * この変換ロジックは rust-core（`terminal_special_key_bytes`/`terminal_ctrl_byte`等）にも
 * 移植され、iOS 版 `TerminalKeyMapper.swift` はそちらへ委譲している（Android/iOS 共通化）。
 * ここを Rust 側へ委譲しないのは、JVM/Robolectric 単体テストがホスト JVM 上でネイティブ
 * ライブラリ（arm64 向けの `.so` のみビルドされる）を解決できないため（`TerminalThemeTest.kt`
 * と同じ制約）。golden テスト（`TerminalKeyEncoderTest.kt`）で両実装の等価性を担保する。
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
    // android.view.KeyEvent.KEYCODE_F1..KEYCODE_F12 と同値（変更不可の Android API 値）
    const val KC_F1  = 131
    const val KC_F2  = 132
    const val KC_F3  = 133
    const val KC_F4  = 134
    const val KC_F5  = 135
    const val KC_F6  = 136
    const val KC_F7  = 137
    const val KC_F8  = 138
    const val KC_F9  = 139
    const val KC_F10 = 140
    const val KC_F11 = 141
    const val KC_F12 = 142

    // JIS配列固有キー(android.view.KeyEvent.KEYCODE_YEN / KEYCODE_RO と同値)。
    // US配列キーボードにはこれらの物理キー自体が存在しないため、通常のキー入力を
    // 誤って横取りすることはない([KeyboardLayoutDetector]参照)。
    const val KC_YEN = 143
    const val KC_RO  = 214

    /**
     * 特殊キーのバイト列。未定義なら null。
     * applicationCursorMode=true のとき矢印キーは SS3 シーケンス（vim 等で必要）。
     * F1〜F4 は常に SS3（`ESC O P`等）、F5〜F12 は CSI `~`形式（xterm 互換、`rust-core`の
     * `terminal_function_key_bytes()`と同一シーケンス）。
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
        KC_F1         -> byteArrayOf(0x1B, 0x4F, 0x50)                         // ESC O P
        KC_F2         -> byteArrayOf(0x1B, 0x4F, 0x51)                         // ESC O Q
        KC_F3         -> byteArrayOf(0x1B, 0x4F, 0x52)                         // ESC O R
        KC_F4         -> byteArrayOf(0x1B, 0x4F, 0x53)                         // ESC O S
        KC_F5         -> byteArrayOf(0x1B, 0x5B, 0x31, 0x35, 0x7E)             // ESC[15~
        KC_F6         -> byteArrayOf(0x1B, 0x5B, 0x31, 0x37, 0x7E)             // ESC[17~
        KC_F7         -> byteArrayOf(0x1B, 0x5B, 0x31, 0x38, 0x7E)             // ESC[18~
        KC_F8         -> byteArrayOf(0x1B, 0x5B, 0x31, 0x39, 0x7E)             // ESC[19~
        KC_F9         -> byteArrayOf(0x1B, 0x5B, 0x32, 0x30, 0x7E)             // ESC[20~
        KC_F10        -> byteArrayOf(0x1B, 0x5B, 0x32, 0x31, 0x7E)             // ESC[21~
        KC_F11        -> byteArrayOf(0x1B, 0x5B, 0x32, 0x33, 0x7E)             // ESC[23~
        KC_F12        -> byteArrayOf(0x1B, 0x5B, 0x32, 0x34, 0x7E)             // ESC[24~
        else          -> null
    }

    /**
     * JIS配列固有キー(¥キー/ろキー)のバイト列。JIS配列と判定/選択されている場合のみ
     * 呼び出し側（[KeyboardLayoutDetector.resolveJisLayout]）が使う。対象外のキーコードは null。
     *
     * Android標準の`KeyCharacterMap`はこの2キーにUnicode文字を割り当てていないことが多く
     * （仮名入力の機能キー切替に使われる想定で、ASCII/直接入力モードでは
     * `getUnicodeChar()`が0を返し無反応になる）、ASCII端末での慣習に合わせて明示的に
     * バックスラッシュ位置へマッピングする:
     * - ¥キー: 単独→`\`(0x5C)、Shift併用→`|`(0x7C)
     * - ろキー: 単独→`\`(0x5C)、Shift併用→`_`(0x5F)
     */
    fun jisSpecialKeyBytes(keyCode: Int, shiftPressed: Boolean): ByteArray? = when (keyCode) {
        KC_YEN -> byteArrayOf(if (shiftPressed) 0x7C else 0x5C)
        KC_RO  -> byteArrayOf(if (shiftPressed) 0x5F else 0x5C)
        else   -> null
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
     * トグル式 Ctrl キー用: 1 コードポイント→Ctrl+<key> の制御コード。
     * 変換できない入力（数字・日本語・複数コードポイント等）は null を返し、
     * 呼び出し側は変換せず元の入力をそのまま送信する。
     *
     * - a-z / A-Z → 0x01-0x1A (Ctrl+A=0x01 ... Ctrl+Z=0x1A)
     * - @ [ \ ] ^ _ (0x40-0x5F) → その 5 bit 下位（Ctrl+@=0x00, Ctrl+[=ESC=0x1B 等）
     * - ? (0x3F) → 0x7F (DEL)
     * - スペース (0x20) → 0x00 (NUL)
     * - 上記以外は null
     */
    fun ctrlByte(codePoint: Int): ByteArray? {
        if (codePoint !in 0x20..0x7F) return null
        val ch = codePoint.toChar()
        return when {
            ch in 'a'..'z' || ch in 'A'..'Z' -> byteArrayOf((ch.uppercaseChar().code and 0x1F).toByte())
            codePoint in 0x40..0x5F -> byteArrayOf((codePoint and 0x1F).toByte())
            ch == '?' -> byteArrayOf(0x7F.toByte())
            ch == ' ' -> byteArrayOf(0x00)
            else -> null
        }
    }

    /**
     * IME 確定テキスト／クリップボードペーストのテキスト→バイト列。
     * 複数コードポイントかつ bracketedPasteMode が有効な場合のみブラケットペーストで囲む。
     * サロゲートペア（絵文字等）を正しく 1 コードポイントとして扱うため codePointCount を使用。
     *
     * 改行正規化（"\r\n" / "\n" → "\r"）はここに集約する。IME 経路(commitText)と
     * クリップボードペースト経路の両方がこの関数を通るため、二重に正規化されることはない。
     */
    fun commitTextBytes(text: String, bracketedPasteMode: Boolean = false): ByteArray {
        if (text.isEmpty()) return ByteArray(0)
        val normalized = text.replace("\r\n", "\r").replace("\n", "\r")
        val codePoints = normalized.codePointCount(0, normalized.length)
        return if (codePoints > 1 && bracketedPasteMode) {
            byteArrayOf(0x1B, 0x5B, 0x32, 0x30, 0x30, 0x7E) +  // ESC[200~
            normalized.toByteArray(Charsets.UTF_8) +
            byteArrayOf(0x1B, 0x5B, 0x32, 0x30, 0x31, 0x7E)    // ESC[201~
        } else {
            normalized.toByteArray(Charsets.UTF_8)
        }
    }
}
