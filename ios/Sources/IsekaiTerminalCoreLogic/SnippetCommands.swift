import Foundation

/// Phase 1G-1(#53): スニペットのコマンド文字列をターミナルへ送信するバイト列に変換する
/// 純粋関数。Android版`SnippetCommands.toBytes`と対称(Rust/UIに依存しないため単体テストが容易)。
/// GRDBの`Snippet`レコード型に依存するオーバーロードは`IsekaiTerminalCore`側
/// (`Sources/IsekaiTerminalCore/ProfileDatabase.swift`)の`extension`で追加している。
public enum SnippetCommands {
    /// `command`の各行区切り(`\n`, `\r\n`)を`\r`(CR)に正規化してUTF-8バイト列にする。
    /// `appendNewline`がtrueかつ末尾がまだ`\r`で終わっていなければ、末尾にも`\r`を追加する
    /// (=最後の行もEnterされる)。falseなら最後の行はEnterされずに残る。
    public static func toBytes(command: String, appendNewline: Bool = true) -> Data {
        guard !command.isEmpty else { return Data() }
        let normalized = command.replacingOccurrences(of: "\r\n", with: "\n").replacingOccurrences(of: "\n", with: "\r")
        let withTrailing = (appendNewline && !normalized.hasSuffix("\r")) ? normalized + "\r" : normalized
        return Data(withTrailing.utf8)
    }
}
