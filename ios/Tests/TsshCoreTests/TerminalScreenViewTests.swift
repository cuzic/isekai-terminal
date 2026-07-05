import XCTest
@testable import TsshCore

/// Phase 1D(#18b): `TerminalScreenView`が`ScreenUpdate`を受け取ってクラッシュせず
/// 描画できることのスモークテスト(実際のピクセル出力の目視確認は対象外)。
final class TerminalScreenViewTests: XCTestCase {
    func testApplyAndDrawDoesNotCrash() {
        let view = TerminalScreenView(frame: CGRect(x: 0, y: 0, width: 400, height: 300))

        let cells = (0..<(4 * 2)).map { i in
            CellData(ch: i % 2 == 0 ? "A" : " ", fg: 0xFFFFFFFF, bg: 0xFF000000, bold: false)
        }
        let update = ScreenUpdate(
            cols: 4, rows: 2, cells: cells,
            cursorRow: 0, cursorCol: 1,
            title: nil, applicationCursorMode: false, bracketedPasteMode: false
        )

        view.apply(update)

        // `layer.render(in:)`はキャッシュ済みcontentsの再生であって`draw(_:)`を
        // 保証しないため、`UIGraphicsImageRenderer`のコンテキスト内で`draw(_:)`を
        // 直接呼び、カスタム描画コードそのものを実行させる。
        view.layoutIfNeeded()
        let renderer = UIGraphicsImageRenderer(size: view.bounds.size)
        _ = renderer.image { _ in
            view.draw(view.bounds)
        }
    }

    func testApplyIgnoresMismatchedCellCountWithoutCrashing() {
        let view = TerminalScreenView(frame: CGRect(x: 0, y: 0, width: 100, height: 100))
        let update = ScreenUpdate(
            cols: 10, rows: 10, cells: [],
            cursorRow: 0, cursorCol: 0,
            title: nil, applicationCursorMode: false, bracketedPasteMode: false
        )
        view.apply(update)
        view.layoutIfNeeded()
        let renderer = UIGraphicsImageRenderer(size: view.bounds.size)
        _ = renderer.image { _ in view.draw(view.bounds) }
    }
}
