package tools.isekai.terminal.input

import uniffi.isekai_terminal_core.TerminalKeyModifiers

/**
 * キーコード→バイト列の変換ロジック。
 * Android 依存なし（定数は android.view.KeyEvent と一致させている）。
 *
 * この変換ロジックは rust-core（`terminal_special_key_bytes`/`terminal_ctrl_byte`等）にも
 * 移植され、iOS 版 `TerminalKeyMapper.swift` はそちらへ委譲している（Android/iOS 共通化）。
 * ここを Rust 側へ委譲しないのは、JVM/Robolectric 単体テストがホスト JVM 上でネイティブ
 * ライブラリ（arm64 向けの `.so` のみビルドされる）を解決できないため（`TerminalThemeTest.kt`
 * と同じ制約）。golden テスト（`TerminalKeyEncoderTest.kt`）で両実装の等価性を担保する
 * （タスク#29の`TerminalKeyModifiers`修飾子拡張も含む）。`TerminalKeyModifiers`型自体は
 * UniFFI生成のプレーンな data class（ネイティブ呼び出しを伴わない）なのでインポートしてよい
 * （他の Robolectric テストも同種の生成型を native 非依存に使っている前例と同じ）。
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
    // android.view.KeyEvent.KEYCODE_FORWARD_DEL / KEYCODE_INSERT と同値（変更不可の Android API 値）。
    // KC_DELは実質バックスペース(0x7F)であり、こちらの前方削除キー(forward delete)とは別物
    // (rust-core `TerminalSpecialKey::Delete` / `::ForwardDelete`の使い分けと同一、タスク#83で
    // テンキーNumLock OFF時の`0`/`.`用に追加)。
    const val KC_FORWARD_DEL = 112
    const val KC_INSERT      = 124
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

    // テンキー(numpad、android.view.KeyEvent.KEYCODE_NUMPAD_* と同値、タスク#43)。
    // 外付けハードウェアキーボード専用(ソフトキーボードはこれらのkeycodeを出さない)。
    const val KC_NUMPAD_0        = 144
    const val KC_NUMPAD_1        = 145
    const val KC_NUMPAD_2        = 146
    const val KC_NUMPAD_3        = 147
    const val KC_NUMPAD_4        = 148
    const val KC_NUMPAD_5        = 149
    const val KC_NUMPAD_6        = 150
    const val KC_NUMPAD_7        = 151
    const val KC_NUMPAD_8        = 152
    const val KC_NUMPAD_9        = 153
    const val KC_NUMPAD_DIVIDE   = 154
    const val KC_NUMPAD_MULTIPLY = 155
    const val KC_NUMPAD_SUBTRACT = 156
    const val KC_NUMPAD_ADD      = 157
    const val KC_NUMPAD_DOT      = 158
    const val KC_NUMPAD_COMMA    = 159
    const val KC_NUMPAD_ENTER    = 160
    const val KC_NUMPAD_EQUALS   = 161
    // KEYCODE_NUMPAD_LEFT_PAREN(162)/KEYCODE_NUMPAD_RIGHT_PAREN(163)は対象外。
    // VT220の物理keypadに存在せず、DECKPAM/DECKPNMどちらのモードでも常に通常の
    // Unicode文字経路([unicodeCharBytes])で`(`/`)`をそのまま送るため、ここに
    // 専用エントリを持つ必要が無い(rust-core`TerminalNumpadKey`のdocコメント参照)。

    private fun TerminalKeyModifiers.isNone(): Boolean = !shift && !alt && !ctrl && !meta

    /**
     * xterm互換の修飾子パラメータ値: `1 + Shift(1) + Alt(2) + Ctrl(4) + Meta(8)`。
     * `rust-core`の`TerminalKeyModifiers::xterm_param()`と同一（golden テストで検証）。
     * 修飾なしの場合は呼び出し側で`isNone()`により別扱い(このメソッドは呼ばれない)。
     */
    private fun xtermParam(m: TerminalKeyModifiers): Int =
        1 + (if (m.shift) 1 else 0) + (if (m.alt) 2 else 0) + (if (m.ctrl) 4 else 0) + (if (m.meta) 8 else 0)

    /** `ESC [ 1 ; <mod> <letter>`（xterm互換の修飾子付きCSI形式）。`terminal_csi_modified`と同一。 */
    private fun csiModified(letter: Byte, modifiers: TerminalKeyModifiers): ByteArray =
        byteArrayOf(0x1B, 0x5B, 0x31, 0x3B) +
            xtermParam(modifiers).toString().toByteArray(Charsets.US_ASCII) +
            byteArrayOf(letter)

    /** `ESC [ <n> ~`（修飾子無し）、または`ESC [ <n> ; <mod> ~`（修飾子有り）。`terminal_tilde_bytes`と同一。 */
    private fun tildeBytes(n: Int, modifiers: TerminalKeyModifiers): ByteArray {
        val body = if (modifiers.isNone()) "\u001B[$n~" else "\u001B[$n;${xtermParam(modifiers)}~"
        return body.toByteArray(Charsets.US_ASCII)
    }

    /**
     * 矢印キー1つ分のバイト列。修飾子が一切無い場合のみ`applicationCursorMode`(DECCKM)に従い
     * SS3/CSI形式を切り替える。修飾子が1つでも付いている場合はDECCKMの値に関わらず常にCSI形式
     * (`ESC[1;5A`等)になる（`terminal_arrow_bytes`と同一ロジック）。
     */
    private fun arrowBytes(letter: Byte, applicationCursorMode: Boolean, modifiers: TerminalKeyModifiers): ByteArray =
        if (modifiers.isNone()) {
            if (applicationCursorMode) byteArrayOf(0x1B, 0x4F, letter) else byteArrayOf(0x1B, 0x5B, letter)
        } else {
            csiModified(letter, modifiers)
        }

    /** Home/End 1つ分のバイト列。`terminal_home_end_bytes`と同一ロジック。 */
    private fun homeEndBytes(letter: Byte, modifiers: TerminalKeyModifiers): ByteArray =
        if (modifiers.isNone()) byteArrayOf(0x1B, 0x5B, letter) else csiModified(letter, modifiers)

    /**
     * Tabのバイト列。修飾子無しなら`0x09`だが、Shift単独の場合はCBT(Cursor Backward Tab、
     * `ESC[Z`)を返す。Shift以外の修飾子(Ctrl+Tab等)は無修飾のTabとして扱う
     * （`terminal_tab_bytes`と同一ロジック）。
     */
    private fun tabBytes(modifiers: TerminalKeyModifiers): ByteArray =
        if (modifiers.shift && !modifiers.ctrl && !modifiers.alt && !modifiers.meta) {
            byteArrayOf(0x1B, 0x5B, 0x5A) // ESC[Z (CBT)
        } else {
            byteArrayOf(0x09)
        }

    /**
     * F1〜F4 1つ分のバイト列。修飾子無しならSS3形式(`ESC O P`等)、修飾子が付くとSS3では
     * 修飾子パラメータを表現できないためCSI形式に切り替わる(`ESC[1;5P`等、
     * `terminal_function_key_bytes`のF1〜F4分岐と同一ロジック)。
     */
    private fun functionKey1to4Bytes(letter: Byte, modifiers: TerminalKeyModifiers): ByteArray =
        if (modifiers.isNone()) byteArrayOf(0x1B, 0x4F, letter) else csiModified(letter, modifiers)

    /**
     * Kitty keyboard protocol(タスク#54)のprogressive enhancement flagsのうちbit0
     * (disambiguate escape codes)。`rust-core`の`KITTY_DISAMBIGUATE_ESCAPE_CODES`
     * (`lib.rs`)と同一値。
     */
    private const val KITTY_DISAMBIGUATE_ESCAPE_CODES: UInt = 0b1u

    /**
     * Escapeキーのバイト列。`kittyFlags`にbit0(disambiguate escape codes)が立っている
     * 場合のみKitty `CSI u`形式(`ESC[27u`)になる(`rust-core`の`terminal_special_key_bytes`
     * のEscape分岐と同一ロジック、タスク#72——Escapeバイト`0x1B`自体が任意のエスケープ
     * シーケンスの開始バイトと衝突しうるため、Kitty仕様がこのbitの名指しする典型例として
     * 無条件でCSI u化するよう定めている)。それ以外(flags=0または他bitのみ)は従来通り
     * 生の`0x1B`。
     */
    private fun escapeBytes(kittyFlags: UShort): ByteArray =
        if (kittyFlags.toUInt() and KITTY_DISAMBIGUATE_ESCAPE_CODES != 0u) {
            byteArrayOf(0x1B, 0x5B, 0x32, 0x37, 0x75) // ESC[27u
        } else {
            byteArrayOf(0x1B)
        }

    /**
     * Kitty keyboard protocol(タスク#54/#72)のbit0(disambiguate escape codes)有効時、
     * Ctrl/Alt(/その組み合わせ・Shift+Alt)付きの印字可能文字キーをCSI u形式
     * (`ESC[<codepoint>;<modifier>u`)へエンコードする(タスク#91、`rust-core`の
     * `terminal_kitty_disambiguated_key_bytes`と同一ロジック)。
     *
     * - [codePoint]はキーの無修飾時の基本コードポイント(`event.getUnicodeChar(0)`が返す値)を
     *   渡すこと。呼び出し側で大文字/小文字を判定する必要はない(この関数が小文字化する)。
     * - `modifier`はxterm/kitty共通のエンコード: `1 + shift(1) + alt(2) + ctrl(4) + meta(8)`。
     * - bit0が立っていない場合、[codePoint]が印字可能文字でない場合、Ctrl/Altのどちらも
     *   押されていない場合は`null`を返す——呼び出し側は既存の`ctrlByte`(legacy Ctrl)や
     *   `altKeyBytes`(legacy Alt)へフォールバックすること。
     * - Kitty仕様上の例外キー(Enter/Tab/Backspace)は`specialKeyBytes`が別途処理するため
     *   この関数の対象外(呼び出し側で特殊キー判定をこの関数より先に行うこと)。
     */
    fun kittyDisambiguatedKeyBytes(codePoint: Int, modifiers: TerminalKeyModifiers, kittyFlags: UShort): ByteArray? {
        if (kittyFlags.toUInt() and KITTY_DISAMBIGUATE_ESCAPE_CODES == 0u) return null
        if (!modifiers.ctrl && !modifiers.alt) return null
        if (codePoint == 0) return null
        val ch = codePoint.toChar()
        if (!(ch.code in 0x21..0x7E) && ch != ' ') return null
        val base = ch.lowercaseChar().code
        var modifierValue = 1
        if (modifiers.shift) modifierValue += 1
        if (modifiers.alt) modifierValue += 2
        if (modifiers.ctrl) modifierValue += 4
        if (modifiers.meta) modifierValue += 8
        return byteArrayOf(0x1B) + "[$base;${modifierValue}u".toByteArray(Charsets.US_ASCII)
    }

    /**
     * 特殊キーのバイト列。未定義なら null。
     * applicationCursorMode=true のとき矢印キーは SS3 シーケンス（vim 等で必要）。
     * F1〜F4 は常に SS3（`ESC O P`等）、F5〜F12 は CSI `~`形式（xterm 互換、`rust-core`の
     * `terminal_function_key_bytes()`と同一シーケンス）。
     * applicationKeypadMode=true のときテンキー(KC_NUMPAD_*)は SS3 シーケンス
     * （DECKPAM/DECKPNM、タスク#43、`rust-core`の`terminal_numpad_key_bytes()`と
     * 同一マッピング）。既定 false(numeric keypad mode)ではリテラル文字/Enterを送る。
     * `modifiers`(Shift/Alt/Ctrl/Meta)は矢印・Home/End・PageUp/Down・F1〜F12・Tabの
     * シーケンスに反映される（`rust-core`の`terminal_special_key_bytes`(タスク#29)と
     * 同一golden表、テンキーには影響しない）。省略時は修飾なし（既存呼び出し元との後方互換）。
     * KC_INSERT/KC_FORWARD_DELは常に`ESC[2~`/`ESC[3~`(rust-coreの`TerminalSpecialKey::ForwardDelete`
     * と同一シーケンス、タスク#83でテンキーNumLock OFF時の`0`/`.`用に追加)。
     * `kittyFlags`(`ScreenUpdate.kittyKeyboardFlags`の最新値、呼び出し側は毎回そのまま渡す
     * こと)はEscapeキーのみに影響する(タスク#72、[escapeBytes]参照)。矢印・Home/End・
     * PageUp/PageDown・F1〜F12・Enter/Tab/Delete/ForwardDeleteはKitty仕様上変更不要
     * (`rust-core`の`terminal_special_key_bytes`のdocコメント参照)。省略時は0(legacy mode)。
     */
    fun specialKeyBytes(
        keyCode: Int,
        applicationCursorMode: Boolean = false,
        applicationKeypadMode: Boolean = false,
        // 毎回新規生成する: `TerminalKeyModifiers`はUniFFI生成の`var`フィールドを持つmutable
        // data classなので、共有インスタンスをデフォルト引数にすると呼び出し側の書き換えが
        // グローバルな既定挙動を壊しかねない(codexレビュー指摘)。
        modifiers: TerminalKeyModifiers = TerminalKeyModifiers(shift = false, alt = false, ctrl = false, meta = false),
        kittyFlags: UShort = 0u,
    ): ByteArray? = when (keyCode) {
        KC_ENTER      -> byteArrayOf(0x0D)
        KC_DEL        -> byteArrayOf(0x7F)
        KC_TAB        -> tabBytes(modifiers)
        KC_ESCAPE     -> escapeBytes(kittyFlags)
        KC_DPAD_UP    -> arrowBytes(0x41, applicationCursorMode, modifiers)
        KC_DPAD_DOWN  -> arrowBytes(0x42, applicationCursorMode, modifiers)
        KC_DPAD_RIGHT -> arrowBytes(0x43, applicationCursorMode, modifiers)
        KC_DPAD_LEFT  -> arrowBytes(0x44, applicationCursorMode, modifiers)
        KC_PAGE_UP    -> tildeBytes(5, modifiers)
        KC_PAGE_DOWN  -> tildeBytes(6, modifiers)
        KC_MOVE_HOME  -> homeEndBytes(0x48, modifiers)
        KC_MOVE_END   -> homeEndBytes(0x46, modifiers)
        KC_INSERT      -> tildeBytes(2, modifiers)                             // ESC[2~
        KC_FORWARD_DEL -> tildeBytes(3, modifiers)                             // ESC[3~（rust-core `TerminalSpecialKey::ForwardDelete`と同一）
        KC_F1         -> functionKey1to4Bytes(0x50, modifiers)                 // ESC O P
        KC_F2         -> functionKey1to4Bytes(0x51, modifiers)                 // ESC O Q
        KC_F3         -> functionKey1to4Bytes(0x52, modifiers)                 // ESC O R
        KC_F4         -> functionKey1to4Bytes(0x53, modifiers)                 // ESC O S
        KC_F5         -> tildeBytes(15, modifiers)                             // ESC[15~
        KC_F6         -> tildeBytes(17, modifiers)                             // ESC[17~
        KC_F7         -> tildeBytes(18, modifiers)                             // ESC[18~
        KC_F8         -> tildeBytes(19, modifiers)                             // ESC[19~
        KC_F9         -> tildeBytes(20, modifiers)                             // ESC[20~
        KC_F10        -> tildeBytes(21, modifiers)                             // ESC[21~
        KC_F11        -> tildeBytes(23, modifiers)                             // ESC[23~
        KC_F12        -> tildeBytes(24, modifiers)                             // ESC[24~
        // テンキー(DECKPAM/DECKPNM、タスク#43)。SS3の最終バイトはVT220/xtermの
        // application keypadテーブルに準拠(rust-core `terminal_numpad_key_bytes()`と
        // 同一マッピング、`numpadBytes`のdocコメント参照)。
        KC_NUMPAD_0        -> numpadBytes(0x70, 0x30, applicationKeypadMode) // p / '0'
        KC_NUMPAD_1        -> numpadBytes(0x71, 0x31, applicationKeypadMode) // q / '1'
        KC_NUMPAD_2        -> numpadBytes(0x72, 0x32, applicationKeypadMode) // r / '2'
        KC_NUMPAD_3        -> numpadBytes(0x73, 0x33, applicationKeypadMode) // s / '3'
        KC_NUMPAD_4        -> numpadBytes(0x74, 0x34, applicationKeypadMode) // t / '4'
        KC_NUMPAD_5        -> numpadBytes(0x75, 0x35, applicationKeypadMode) // u / '5'
        KC_NUMPAD_6        -> numpadBytes(0x76, 0x36, applicationKeypadMode) // v / '6'
        KC_NUMPAD_7        -> numpadBytes(0x77, 0x37, applicationKeypadMode) // w / '7'
        KC_NUMPAD_8        -> numpadBytes(0x78, 0x38, applicationKeypadMode) // x / '8'
        KC_NUMPAD_9        -> numpadBytes(0x79, 0x39, applicationKeypadMode) // y / '9'
        KC_NUMPAD_DOT      -> numpadBytes(0x6E, 0x2E, applicationKeypadMode) // n / '.'
        KC_NUMPAD_COMMA    -> numpadBytes(0x6C, 0x2C, applicationKeypadMode) // l / ','
        KC_NUMPAD_ADD      -> numpadBytes(0x6B, 0x2B, applicationKeypadMode) // k / '+'
        KC_NUMPAD_SUBTRACT -> numpadBytes(0x6D, 0x2D, applicationKeypadMode) // m / '-'
        KC_NUMPAD_MULTIPLY -> numpadBytes(0x6A, 0x2A, applicationKeypadMode) // j / '*'
        KC_NUMPAD_DIVIDE   -> numpadBytes(0x6F, 0x2F, applicationKeypadMode) // o / '/'
        KC_NUMPAD_EQUALS   -> numpadBytes(0x58, 0x3D, applicationKeypadMode) // X / '='
        KC_NUMPAD_ENTER    -> if (applicationKeypadMode) byteArrayOf(0x1B, 0x4F, 0x4D) else byteArrayOf(0x0D)
        else          -> null
    }

    /**
     * テンキー1キー分のバイト列。`applicationKeypadMode`(DECKPAM)がtrueなら
     * `ESC O <ss3Letter>`(SS3)、falseなら`normalByte`単体を返す共通ヘルパー
     * (`terminal_arrow_bytes`と同型のパターン)。
     */
    private fun numpadBytes(ss3Letter: Int, normalByte: Int, applicationKeypadMode: Boolean): ByteArray =
        if (applicationKeypadMode) byteArrayOf(0x1B, 0x4F, ss3Letter.toByte()) else byteArrayOf(normalByte.toByte())

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
     * 物理 Alt(Meta)キー併用時: ESC プレフィックスを付与する。xterm 等の
     * "Meta sends escape"(`altSendsEscape`)相当で、readline/vim 等の Alt+<key> ショートカット
     * (Alt+b/Alt+f で単語単位移動、等)をターミナル側アプリへそのまま伝えるための標準的な変換。
     * unicodeChar が 0 なら null。
     */
    fun altKeyBytes(unicodeChar: Int): ByteArray? {
        val base = unicodeCharBytes(unicodeChar) ?: return null
        return byteArrayOf(0x1B) + base
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
