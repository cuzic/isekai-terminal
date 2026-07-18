import Foundation

/// OSC 8(タスク#40)ハイパーリンクのタップUI(タスク#52)向けの純粋ロジック。
/// Android版`TerminalHyperlink.kt`と対称。
///
/// hit-test自体(画面座標→セル→リンクURL)は「UI表示だけに閉じた判断」であり
/// rust-ssot原則(`.claude/rules/rust-ssot.md`)の対象外——Rust側は既に
/// `CellData.linkId`/`ScreenUpdate.linkTable`としてintern済みのURL文字列を
/// 公開済みで、ここではその配列を読むだけで新しい状態機械は作らない。
///
/// スキーム許可判定(`isOpenableHyperlinkScheme`)はセキュリティポリシーであり、
/// 接続先ホストの出力(=信頼できない入力)を無条件で`UIApplication.open`へ渡さない
/// ための独立した純粋関数(タスク#52 Fableレビュー2次の要件、Android版
/// `RemoteClipboardPolicy`と同じ考え方)。呼び出し元(`TerminalScreenView.swift`)は、
/// タップされたセルにリンクがあり、かつスキームが許可リストに含まれる場合のみ
/// URL全文の確認ダイアログを表示し、ユーザーが確認した場合のみ`UIApplication.open`を呼ぶ。

/// `update`の(`row`, `col`)セル(0-indexed)が指すハイパーリンクURLを返す。
/// リンクなし・範囲外・`linkId`が`linkTable`の範囲外(本来起こらないはずだが、
/// 呼び出し側からの防御的な扱いとしてクラッシュではなくnilを返す)の場合はnil。
public func linkURL(at update: ScreenUpdate, row: Int, col: Int) -> String? {
    let cols = Int(update.cols)
    let rows = Int(update.rows)
    guard cols > 0, rows > 0, row >= 0, row < rows, col >= 0, col < cols else { return nil }
    let index = row * cols + col
    guard index >= 0, index < update.cells.count else { return nil }
    guard let linkId = update.cells[index].linkId else { return nil }
    let tableIndex = Int(linkId)
    guard tableIndex >= 0, tableIndex < update.linkTable.count else { return nil }
    return update.linkTable[tableIndex]
}

/// URL文字列の先頭にある`scheme:`を抽出する(RFC 3986 3.1の生成規則に準拠)。
private let hyperlinkSchemePattern = try? NSRegularExpression(pattern: "^([A-Za-z][A-Za-z0-9+.-]*):")

/// リモート(信頼できないホスト出力)由来のURLを、タップ時に`UIApplication.open`へ
/// 渡して安全に開いてよいかどうか。`http`/`https`のみ許可する——`intent://`相当の
/// 任意コンポーネント起動や`file://`によるローカルファイル露出、`javascript:`の
/// スクリプト実行に悪用されうるため、無条件で開かない
/// (タスク#52 Fableレビュー2次のセキュリティ要件)。
public func isOpenableHyperlinkScheme(_ url: String) -> Bool {
    guard let regex = hyperlinkSchemePattern else { return false }
    let range = NSRange(url.startIndex..<url.endIndex, in: url)
    guard let match = regex.firstMatch(in: url, range: range),
          let schemeRange = Range(match.range(at: 1), in: url) else { return false }
    let scheme = url[schemeRange].lowercased()
    return scheme == "http" || scheme == "https"
}
