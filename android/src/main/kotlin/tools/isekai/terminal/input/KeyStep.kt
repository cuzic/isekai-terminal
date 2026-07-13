package tools.isekai.terminal.input

/**
 * 打鍵列(KeySequence)を構成する最小単位。
 *
 * バイト列への変換ロジックは持たない(このデータクラス自体は Android/Rust いずれにも非依存)。
 * 変換は [tools.isekai.terminal.KeySequenceCommands.toBytes] が既存の [TerminalKeyEncoder] へ
 * 委譲する形で行う(打鍵列専用の変換ロジックを新たに作らない)。
 */
sealed class KeyStep {
    /** Ctrl+<英字> 相当の制御バイト。[TerminalKeyEncoder.ctrlByte] へ委譲する。 */
    data class CtrlChar(val char: Char) : KeyStep()

    /** リテラルテキスト。[TerminalKeyEncoder.commitTextBytes] へ委譲する(改行は `\r` に正規化)。 */
    data class Text(val text: String) : KeyStep()

    /**
     * 特殊キー。ペイロードは新規 enum を作らず、既存の [TerminalKeyEncoder] の `KC_*` 定数
     * (android.view.KeyEvent 互換の Int keyCode)をそのまま使う。
     */
    data class Special(val keyCode: Int) : KeyStep()

    /**
     * 打鍵列セット(パック)のテンプレート内でのみ使用するプレースホルダー参照(例: `{prefix}`)。
     * [tools.isekai.terminal.KeySequenceCommands.toBytes] に渡す前に、呼び出し側が
     * パックのインストール値で具体的な [KeyStep] へ解決しておくこと。未解決のまま渡された
     * 場合は何もバイトを出力しない(呼び出し側の実装ミスに対する防御であり、正常系では
     * 発生しない想定)。
     */
    data class PlaceholderRef(val name: String) : KeyStep()
}
