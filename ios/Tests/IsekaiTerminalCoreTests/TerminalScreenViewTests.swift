import XCTest
@testable import IsekaiTerminalCore
import IsekaiTerminalCoreLogic

/// Phase 1D(#18b): `TerminalScreenView`が`ScreenUpdate`を受け取ってクラッシュせず
/// 描画できることのスモークテスト(実際のピクセル出力の目視確認は対象外)。
final class TerminalScreenViewTests: XCTestCase {
    func testApplyAndDrawDoesNotCrash() {
        let view = TerminalScreenView(frame: CGRect(x: 0, y: 0, width: 400, height: 300))

        let cells = (0..<(4 * 2)).map { i in
            CellData(
                ch: i % 2 == 0 ? "A" : " ", fg: 0xFFFFFFFF, bg: 0xFF000000, bold: false,
                dim: false, italic: false, underline: false,
                strikethrough: false, blink: false, invisible: false, linkId: nil
            )
        }
        let update = ScreenUpdate(
            cols: 4, rows: 2, cells: cells,
            cursorRow: 0, cursorCol: 1,
            title: nil, applicationCursorMode: false, bracketedPasteMode: false,
            mouseReportingMode: .off, sgrMouseMode: false,
            cursorVisible: true, bellGeneration: 0,
            cursorShape: .block, cursorBlink: true, linkTable: [], images: [], kittyKeyboardFlags: 0
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
            title: nil, applicationCursorMode: false, bracketedPasteMode: false,
            mouseReportingMode: .off, sgrMouseMode: false,
            cursorVisible: true, bellGeneration: 0,
            cursorShape: .block, cursorBlink: true, linkTable: [], images: [], kittyKeyboardFlags: 0
        )
        view.apply(update)
        view.layoutIfNeeded()
        let renderer = UIGraphicsImageRenderer(size: view.bounds.size)
        _ = renderer.image { _ in view.draw(view.bounds) }
    }

    // MARK: - Phase 1F-2(#49): clampedFontScale
    //
    // `CGFloat`を使うため`IsekaiTerminalCoreLogic`(Linuxでも`swift test`可能な純ロジック層)には
    // 移していない。純粋ロジック自体の検証は`Tests/IsekaiTerminalCoreLogicTests/TerminalSelectionTests.swift`
    // に集約する方針だが、この関数はCoreGraphics依存のため`TerminalScreenView.swift`
    // (`IsekaiTerminalCore`ターゲット)に残している。

    func testClampedFontScaleAppliesZoomDelta() {
        XCTAssertEqual(clampedFontScale(current: 1.0, zoomDelta: 1.2), 1.2, accuracy: 0.0001)
    }

    func testClampedFontScaleClampsToMinimum() {
        XCTAssertEqual(clampedFontScale(current: 0.6, zoomDelta: 0.1), 0.5, accuracy: 0.0001)
    }

    func testClampedFontScaleClampsToMaximum() {
        XCTAssertEqual(clampedFontScale(current: 2.9, zoomDelta: 2.0), 3.0, accuracy: 0.0001)
    }

    // MARK: - タスク#20: 動的resize(view bounds→cols/rows→onSizeChanged)

    func testOnSizeChangedFiresWithComputedColsAndRowsOnLayout() {
        let view = TerminalScreenView(frame: CGRect(x: 0, y: 0, width: 400, height: 300))
        var reported: (cols: UInt32, rows: UInt32)?
        view.onSizeChanged = { cols, rows in reported = (cols, rows) }

        view.setNeedsLayout()
        view.layoutIfNeeded()

        XCTAssertNotNil(reported)
        XCTAssertGreaterThanOrEqual(reported?.cols ?? 0, 10)
        XCTAssertGreaterThanOrEqual(reported?.rows ?? 0, 5)
    }

    func testOnSizeChangedClampsToMinimumForTinyFrame() {
        let view = TerminalScreenView(frame: CGRect(x: 0, y: 0, width: 1, height: 1))
        var reported: (cols: UInt32, rows: UInt32)?
        view.onSizeChanged = { cols, rows in reported = (cols, rows) }

        view.setNeedsLayout()
        view.layoutIfNeeded()

        // Android版`coerceAtLeast(10)`/`coerceAtLeast(5)`と対称の下限。
        XCTAssertEqual(reported?.cols, 10)
        XCTAssertEqual(reported?.rows, 5)
    }

    func testOnSizeChangedDoesNotRefireForUnchangedComputedSize() {
        let view = TerminalScreenView(frame: CGRect(x: 0, y: 0, width: 400, height: 300))
        var callCount = 0
        view.onSizeChanged = { _, _ in callCount += 1 }

        view.setNeedsLayout()
        view.layoutIfNeeded()
        XCTAssertEqual(callCount, 1)

        // boundsが変わらないままlayoutが再度発火しても、算出されたcols/rowsが同じなら
        // 再送しない(Android版`LaunchedEffect(cols, rows, connected)`が値の変化でしか
        // 再発火しないのと対称)。
        view.setNeedsLayout()
        view.layoutIfNeeded()
        XCTAssertEqual(callCount, 1)
    }

    func testResendSizeOnConnectionEstablishedForcesRefireEvenIfUnchanged() {
        let view = TerminalScreenView(frame: CGRect(x: 0, y: 0, width: 400, height: 300))
        var callCount = 0
        view.onSizeChanged = { _, _ in callCount += 1 }

        view.setNeedsLayout()
        view.layoutIfNeeded()
        XCTAssertEqual(callCount, 1)

        // タスク#20: `connect()`は既定の80x24でセッションを開始するため、接続確立の
        // 度に(cols/rowsの値自体が変わっていなくても)実サイズへ確実に一度合わせ直す
        // 必要がある(Android版`LaunchedEffect(cols, rows, connected)`の`connected`
        // キーと対称)。
        view.resendSizeOnConnectionEstablished()
        XCTAssertEqual(callCount, 2)
    }
}
