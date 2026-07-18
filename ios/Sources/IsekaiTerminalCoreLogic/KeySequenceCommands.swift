import Foundation

/// 打鍵列(KeySequence)をターミナルへ送信するバイト列に変換する純粋関数。Android版
/// `KeySequenceCommands.toBytes`と対称(Rust/UIに依存しないため単体テストが容易)。
///
/// 各ステップの実際のバイト変換ロジックはここで再実装せず、既存の型/既存のRust委譲関数へ
/// 委譲するだけにする(3回目のcodexレビュー指摘: Text変換にも既にRust側の共通実装
/// `terminal_commit_text_bytes`があるため、`SnippetCommands.toBytes`のような改行強制付与
/// ロジックを再利用してはいけない — `Text("c")`のような単発キー入力に余計なCRが付いてしまう)。
public enum KeySequenceCommands {
    /// - `.ctrlChar(c)` → `TerminalKeyMapper.controlByte(for:)`(Rust `terminal_ctrl_byte`委譲済み)。
    ///   変換できない文字(数字・日本語等)は何もバイトを出力しない。
    /// - `.special(key)` → `TerminalKeyMapper.bytes(for:applicationCursorMode:)`
    ///   ([applicationCursorMode]を伝播)。
    /// - `.text(s)` → `terminalCommitTextBytes(text:bracketedPasteMode:)`(Rust委譲、UniFFI生成
    ///   関数を直接使用。末尾への改行の強制付与はしない)。
    /// - `.placeholderRef` → 何も出力しない(呼び出し側が事前に具体的な[KeyStep]へ解決して
    ///   おくべきものであり、正常系では到達しない)。
    ///
    /// タスク#82の見落としタスク(codexレビュー指摘)についての方針: `KeyStep`/`SpecialKey`には
    /// テンキー(numpad)を表すケースが無いため、この経路は`applicationKeypadMode`
    /// (DECKPAM)を一切扱わない。`SpecialKey`にnumpadケースが追加されるまでは
    /// バグではなく、打鍵列(KeySequence)機能でテンキーを表現したくなった時点で
    /// `applicationCursorMode`と同じパターン(引数として伝播)を追加すること。
    public static func toBytes(_ steps: [KeyStep], applicationCursorMode: Bool = false) -> Data {
        var out = Data()
        for step in steps {
            switch step {
            case .ctrlChar(let c):
                if let byte = TerminalKeyMapper.controlByte(for: c) {
                    out.append(byte)
                }
            case .special(let key):
                out.append(contentsOf: TerminalKeyMapper.bytes(for: key, applicationCursorMode: applicationCursorMode))
            case .text(let text):
                out.append(terminalCommitTextBytes(text: text, bracketedPasteMode: false))
            case .placeholderRef:
                break
            }
        }
        return out
    }
}
