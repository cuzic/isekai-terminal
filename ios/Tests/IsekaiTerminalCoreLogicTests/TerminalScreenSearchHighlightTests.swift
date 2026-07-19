import XCTest
@testable import IsekaiTerminalCoreLogic

/// タスク#66/#67: `searchHighlightMatch`のピュアな判断ロジックを検証する。
/// Android版`TerminalScreenSearchHighlightTest.kt`(`searchHighlightMatch`)と対称。
///
/// タスク#79: 当初の実装(`TerminalView.swift`にインラインで書かれていた
/// `currentSearchMatch.flatMap { $0.row == 0 ? nil : $0 }`)は`row == 0`を常に除外して
/// いたため、検索結果に`row == 0`のマッチが出てもジャンプ・ハイライトのどちらも到達
/// できない(「検索結果に出るのに到達できない」)UX上のバグがあった。判断ロジックを
/// `IsekaiTerminalCoreLogic`層のピュア関数へ抽出し(以前は`TerminalScreenViewTests`の
/// クラッシュ確認スモークテストでしか間接的にしか検証できなかった)、`showingScrollback`
/// (`jumpToCurrentMatch()`がscrollback最新行へジャンプする際に真にする、`scrollOffset == 0`
/// とは独立したフラグ)を導入して実際にscrollback最新行を表示中であればrow=0のマッチも
/// ハイライトできるようにした。
final class TerminalScreenSearchHighlightTests: XCTestCase {
    func testReturnsNilWhenThereIsNoCurrentMatch() {
        XCTAssertNil(searchHighlightMatch(nil, scrollOffset: 3, showingScrollback: false))
    }

    func testReturnsTheMatchWhenScrollOffsetEqualsItsRow() {
        let match = ScrollbackSearchMatch(row: 3, col: 1, len: 2)
        XCTAssertEqual(searchHighlightMatch(match, scrollOffset: 3, showingScrollback: false), match)
    }

    func testReturnsNilWhenScrollOffsetDoesNotEqualItsRow() {
        let match = ScrollbackSearchMatch(row: 3, col: 1, len: 2)
        XCTAssertNil(searchHighlightMatch(match, scrollOffset: 1, showingScrollback: false))
    }

    func testReturnsNilForRowZeroWhenScrollOffsetIsZeroAndNotShowingScrollback() {
        // ライブ画面表示中(scrollOffset == 0 かつ showingScrollback == false)は、
        // row == 0(scrollback最新行)のマッチを誤ってハイライトしてはいけない
        // (codexレビュー指摘の回帰ケース)。
        let match = ScrollbackSearchMatch(row: 0, col: 1, len: 2)
        XCTAssertNil(searchHighlightMatch(match, scrollOffset: 0, showingScrollback: false))
    }

    func testReturnsTheMatchForRowZeroWhenScrollOffsetIsZeroAndShowingScrollback() {
        // タスク#79の回帰確認: `jumpToCurrentMatch`がscrollback最新行(row=0)へジャンプした
        // 後(showingScrollback = true)は、scrollOffset == 0のままでもそのマッチを正しく
        // ハイライトできなければならない(以前は常にnilを返していた)。
        let match = ScrollbackSearchMatch(row: 0, col: 1, len: 2)
        XCTAssertEqual(searchHighlightMatch(match, scrollOffset: 0, showingScrollback: true), match)
    }

    func testReturnsNilForRowZeroWhenScrollOffsetIsNonzeroEvenIfShowingScrollback() {
        // showingScrollbackが真でも、実際に表示されているoffsetとrowが一致しなければ
        // ハイライトしない(ジャンプ直後でscrollOffsetがまだ追従していない場合の
        // 誤描画を避ける)。
        let match = ScrollbackSearchMatch(row: 0, col: 1, len: 2)
        XCTAssertNil(searchHighlightMatch(match, scrollOffset: 5, showingScrollback: true))
    }
}
