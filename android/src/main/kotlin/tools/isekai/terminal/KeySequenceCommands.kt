package tools.isekai.terminal

import java.io.ByteArrayOutputStream
import tools.isekai.terminal.input.KeyStep
import tools.isekai.terminal.input.TerminalKeyEncoder

/**
 * 打鍵列(KeySequence)をターミナルへ送信するバイト列に変換する純粋関数。
 * [SnippetCommands.toBytes] と同じくAndroid/Rustいずれにも依存しないため単体テストが容易。
 *
 * 各ステップの実際のバイト変換ロジックはここで再実装せず、既存の [TerminalKeyEncoder] へ
 * 委譲するだけにする(`TerminalKeyEncoder.kt` のdocコメントに明記の通り、キー→バイト列変換は
 * Rust[`terminal_ctrl_byte`/`terminal_special_key_bytes`/`terminal_commit_text_bytes`]と
 * Android Kotlinの2箇所に既に存在し、golden testで等価性を担保する運用が確定している。
 * ここに3つ目の実装を作らない)。
 */
object KeySequenceCommands {
    /**
     * [steps] を順番にバイト列へ変換して連結する。
     *
     * - [KeyStep.CtrlChar] → [TerminalKeyEncoder.ctrlByte]。変換できない文字(数字・日本語等)は
     *   何もバイトを出力しない。
     * - [KeyStep.Special] → [TerminalKeyEncoder.specialKeyBytes]([applicationCursorMode]を伝播)。
     *   未知のkeyCodeは何もバイトを出力しない。
     * - [KeyStep.Text] → [TerminalKeyEncoder.commitTextBytes]([SnippetCommands.toBytes]とは違い、
     *   末尾への改行の強制付与はしない。`Text("c")` はそのまま1文字のバイト列になる)。
     * - [KeyStep.PlaceholderRef] → 何も出力しない(呼び出し側が事前に具体的な [KeyStep] へ
     *   解決しておくべきものであり、正常系では到達しない)。
     */
    fun toBytes(
        steps: List<KeyStep>,
        applicationCursorMode: Boolean = false,
        // DECKPAM/DECKPNM(タスク#43)。KeyStep.Specialにテンキー(KC_NUMPAD_*)のkeyCodeが
        // 含まれる場合に必要(TerminalKeyEncoder.specialKeyBytesへそのまま伝播するだけ)。
        applicationKeypadMode: Boolean = false,
    ): ByteArray {
        val out = ByteArrayOutputStream()
        for (step in steps) {
            when (step) {
                is KeyStep.CtrlChar -> TerminalKeyEncoder.ctrlByte(step.char.code)?.let(out::write)
                is KeyStep.Special -> TerminalKeyEncoder.specialKeyBytes(step.keyCode, applicationCursorMode, applicationKeypadMode)?.let(out::write)
                is KeyStep.Text -> out.write(TerminalKeyEncoder.commitTextBytes(step.text, bracketedPasteMode = false))
                is KeyStep.PlaceholderRef -> Unit
            }
        }
        return out.toByteArray()
    }
}
