package tools.isekai.terminal.input

/**
 * 打鍵列編集/一覧UIでチップ・プレビューとして表示する短いラベル。
 * バイト変換([tools.isekai.terminal.KeySequenceCommands.toBytes])とは独立した表示専用ロジック。
 */
fun KeyStep.shortLabel(): String = when (this) {
    is KeyStep.CtrlChar -> "^${char.uppercaseChar()}"
    is KeyStep.Text -> text
    is KeyStep.Special -> specialKeyShortLabel(keyCode)
    is KeyStep.PlaceholderRef -> "{$name}"
}

fun List<KeyStep>.previewText(): String = joinToString(" ") { it.shortLabel() }

/** [SPECIAL_KEY_CHOICES]をkeyCode→labelの逆引きに使う(SSOTはSPECIAL_KEY_CHOICES側、
 *  ここでは2重管理しない)。選択肢一覧に無いkeyCode(将来追加され得るraw値)は
 *  "Key(keyCode)"にフォールバックする。 */
private fun specialKeyShortLabel(keyCode: Int): String =
    SPECIAL_KEY_CHOICES.firstOrNull { it.second == keyCode }?.first ?: "Key($keyCode)"

/** 打鍵列編集画面のステップ追加UIで選べる特殊キーの一覧(ラベル付き)。 */
val SPECIAL_KEY_CHOICES: List<Pair<String, Int>> = listOf(
    "Enter" to TerminalKeyEncoder.KC_ENTER,
    "Del" to TerminalKeyEncoder.KC_DEL,
    "Tab" to TerminalKeyEncoder.KC_TAB,
    "Esc" to TerminalKeyEncoder.KC_ESCAPE,
    "↑" to TerminalKeyEncoder.KC_DPAD_UP,
    "↓" to TerminalKeyEncoder.KC_DPAD_DOWN,
    "←" to TerminalKeyEncoder.KC_DPAD_LEFT,
    "→" to TerminalKeyEncoder.KC_DPAD_RIGHT,
    "PageUp" to TerminalKeyEncoder.KC_PAGE_UP,
    "PageDown" to TerminalKeyEncoder.KC_PAGE_DOWN,
    "Home" to TerminalKeyEncoder.KC_MOVE_HOME,
    "End" to TerminalKeyEncoder.KC_MOVE_END,
    "F1" to TerminalKeyEncoder.KC_F1,
    "F2" to TerminalKeyEncoder.KC_F2,
    "F3" to TerminalKeyEncoder.KC_F3,
    "F4" to TerminalKeyEncoder.KC_F4,
    "F5" to TerminalKeyEncoder.KC_F5,
    "F6" to TerminalKeyEncoder.KC_F6,
    "F7" to TerminalKeyEncoder.KC_F7,
    "F8" to TerminalKeyEncoder.KC_F8,
    "F9" to TerminalKeyEncoder.KC_F9,
    "F10" to TerminalKeyEncoder.KC_F10,
    "F11" to TerminalKeyEncoder.KC_F11,
    "F12" to TerminalKeyEncoder.KC_F12,
)
