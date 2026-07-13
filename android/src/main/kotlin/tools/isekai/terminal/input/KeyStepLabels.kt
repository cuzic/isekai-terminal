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

private fun specialKeyShortLabel(keyCode: Int): String = when (keyCode) {
    TerminalKeyEncoder.KC_ENTER -> "Enter"
    TerminalKeyEncoder.KC_DEL -> "Del"
    TerminalKeyEncoder.KC_TAB -> "Tab"
    TerminalKeyEncoder.KC_ESCAPE -> "Esc"
    TerminalKeyEncoder.KC_DPAD_UP -> "↑"
    TerminalKeyEncoder.KC_DPAD_DOWN -> "↓"
    TerminalKeyEncoder.KC_DPAD_LEFT -> "←"
    TerminalKeyEncoder.KC_DPAD_RIGHT -> "→"
    TerminalKeyEncoder.KC_PAGE_UP -> "PageUp"
    TerminalKeyEncoder.KC_PAGE_DOWN -> "PageDown"
    TerminalKeyEncoder.KC_MOVE_HOME -> "Home"
    TerminalKeyEncoder.KC_MOVE_END -> "End"
    TerminalKeyEncoder.KC_F1 -> "F1"
    TerminalKeyEncoder.KC_F2 -> "F2"
    TerminalKeyEncoder.KC_F3 -> "F3"
    TerminalKeyEncoder.KC_F4 -> "F4"
    TerminalKeyEncoder.KC_F5 -> "F5"
    TerminalKeyEncoder.KC_F6 -> "F6"
    TerminalKeyEncoder.KC_F7 -> "F7"
    TerminalKeyEncoder.KC_F8 -> "F8"
    TerminalKeyEncoder.KC_F9 -> "F9"
    TerminalKeyEncoder.KC_F10 -> "F10"
    TerminalKeyEncoder.KC_F11 -> "F11"
    TerminalKeyEncoder.KC_F12 -> "F12"
    else -> "Key($keyCode)"
}

/** 打鍵列編集画面のステップ追加UIで選べる特殊キーの一覧(ラベル付き)。 */
val SPECIAL_KEY_CHOICES: List<Pair<String, Int>> = listOf(
    "Enter" to TerminalKeyEncoder.KC_ENTER,
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
