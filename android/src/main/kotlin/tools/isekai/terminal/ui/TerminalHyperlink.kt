package tools.isekai.terminal.ui

import uniffi.isekai_terminal_core.ScreenUpdate

/**
 * OSC 8(タスク#40)ハイパーリンクのタップUI(タスク#52)向けの純粋ロジック。
 *
 * hit-test自体(画面座標→セル→リンクURL)は「UI表示だけに閉じた判断」であり
 * rust-ssot原則([.claude/rules/rust-ssot.md])の対象外——Rust側は既に
 * `CellData.linkId`/`ScreenUpdate.linkTable`としてintern済みのURL文字列を
 * 公開済みで、ここではその配列を読むだけで新しい状態機械は作らない。
 *
 * スキーム許可判定([isOpenableHyperlinkScheme])はセキュリティポリシーであり、
 * [RemoteClipboardPolicy]と同じ理由(接続先ホストの出力=信頼できない入力)で
 * 独立した純粋関数として切り出しAndroidフレームワーク非依存でテストする。
 * 呼び出し元(`TerminalScreen.kt`)は、タップされたセルにリンクがあり、かつ
 * スキームが許可リストに含まれる場合のみURL全文の確認ダイアログを表示し、
 * ユーザーが確認した場合のみ`ACTION_VIEW` Intentを発行する。
 */

/**
 * [update]の([row], [col])セル(0-indexed)が指すハイパーリンクURLを返す。
 * リンクなし・範囲外・`linkId`が`linkTable`の範囲外(本来起こらないはずだが、
 * 呼び出し側からの防御的な扱いとしてクラッシュではなくnullを返す)の場合はnull。
 */
fun linkUrlAtCell(update: ScreenUpdate, row: Int, col: Int): String? {
    val cols = update.cols.toInt()
    val rows = update.rows.toInt()
    if (cols <= 0 || rows <= 0 || row !in 0 until rows || col !in 0 until cols) return null
    val cells = update.cells
    val index = row * cols + col
    if (index !in cells.indices) return null
    val linkId = cells[index].linkId ?: return null
    return update.linkTable.getOrNull(linkId.toInt())
}

/** URL文字列の先頭にある `scheme:` を抽出する(RFC 3986 3.1 の生成規則に準拠)。 */
private val SCHEME_REGEX = Regex("^([A-Za-z][A-Za-z0-9+.-]*):")

/**
 * リモート(信頼できないホスト出力)由来のURLを、タップ時に`ACTION_VIEW`へ渡して
 * 安全に開いてよいかどうか。`http`/`https`のみ許可する——`intent://`は任意のAndroid
 * コンポーネント起動、`file://`はローカルファイル露出、`javascript:`はWebView文脈での
 * スクリプト実行に悪用されうるため、無条件で`ACTION_VIEW`へ渡さない
 * (タスク#52 Fableレビュー2次のセキュリティ要件)。
 */
fun isOpenableHyperlinkScheme(url: String): Boolean {
    val scheme = SCHEME_REGEX.find(url)?.groupValues?.get(1)?.lowercase() ?: return false
    return scheme == "http" || scheme == "https"
}
