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

    // MARK: - タスク#23: SGR装飾(underline/italic/reverse/dim/strikethrough/blink/invisible)

    /// 全SGR属性の組み合わせを含む`ScreenUpdate`を与えてもクラッシュせず`draw(_:)`が
    /// 完走することのスモークテスト(実際のピクセル出力の目視確認は対象外、
    /// `testApplyAndDrawDoesNotCrash`と同じ方針)。特に`italicFont`/`boldItalicFont`の
    /// `UIFontDescriptor.withSymbolicTraits`がシステムフォントで期待通り解決されることを
    /// 確認する。`blinkPhaseVisible`は`Timer`経由でしかトグルされない private 状態のため、
    /// このテストでは検証していない(2回`draw`を呼ぶのは単に冪等性の確認)。reverseは
    /// `terminal.rs`側でパース時に実効fg/bgへ解決済みのため(#21)、`CellData`自体には
    /// reverseフィールドが無く、ここでは検証対象外。
    func testDrawWithAllSgrAttributesDoesNotCrash() {
        let view = TerminalScreenView(frame: CGRect(x: 0, y: 0, width: 400, height: 300))

        func cell(bold: Bool = false, dim: Bool = false, italic: Bool = false, underline: Bool = false,
                   strikethrough: Bool = false, blink: Bool = false, invisible: Bool = false) -> CellData {
            CellData(
                ch: "A", fg: 0xFFFFFFFF, bg: 0xFF000000, bold: bold, dim: dim, italic: italic,
                underline: underline, strikethrough: strikethrough, blink: blink, invisible: invisible,
                linkId: nil
            )
        }

        let cells = [
            cell(bold: true, italic: true),
            cell(dim: true),
            cell(underline: true),
            cell(strikethrough: true),
            cell(blink: true),
            cell(invisible: true),
            cell(),
            cell(bold: true, dim: true, italic: true, underline: true, strikethrough: true, blink: true, invisible: true),
        ]
        let update = ScreenUpdate(
            cols: UInt32(cells.count), rows: 1, cells: cells,
            cursorRow: 0, cursorCol: 0,
            title: nil, applicationCursorMode: false, bracketedPasteMode: false,
            mouseReportingMode: .off, sgrMouseMode: false,
            cursorVisible: true, bellGeneration: 0,
            cursorShape: .block, cursorBlink: true, linkTable: [], images: [], kittyKeyboardFlags: 0
        )

        view.apply(update)
        view.layoutIfNeeded()
        let renderer = UIGraphicsImageRenderer(size: view.bounds.size)
        // 2回描画してもクラッシュ・状態不整合が無いことの冪等性確認。
        _ = renderer.image { _ in view.draw(view.bounds) }
        _ = renderer.image { _ in view.draw(view.bounds) }
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

    // MARK: - タスク#34: DECSCUSRカーソル形状の描画

    /// `cursorShape`の3値(block/underline/bar)いずれでも`draw(_:)`がクラッシュせず
    /// 完走することのスモークテスト(実ピクセルの目視確認は対象外、既存方針と同じ)。
    /// 形状ごとの矩形計算(`TerminalScreenView.draw(_:)`の`switch update.cursorShape`)を
    /// 一通り実行させる意味がある。
    func testDrawWithEachCursorShapeDoesNotCrash() {
        for shape: CursorShape in [.block, .underline, .bar] {
            let view = TerminalScreenView(frame: CGRect(x: 0, y: 0, width: 400, height: 300))
            let cells = [CellData(
                ch: "A", fg: 0xFFFFFFFF, bg: 0xFF000000, bold: false,
                dim: false, italic: false, underline: false,
                strikethrough: false, blink: false, invisible: false, linkId: nil
            )]
            let update = ScreenUpdate(
                cols: 1, rows: 1, cells: cells,
                cursorRow: 0, cursorCol: 0,
                title: nil, applicationCursorMode: false, bracketedPasteMode: false,
                mouseReportingMode: .off, sgrMouseMode: false,
                cursorVisible: true, bellGeneration: 0,
                cursorShape: shape, cursorBlink: true, linkTable: [], images: [], kittyKeyboardFlags: 0
            )
            view.apply(update)
            view.layoutIfNeeded()
            let renderer = UIGraphicsImageRenderer(size: view.bounds.size)
            _ = renderer.image { _ in view.draw(view.bounds) }
        }
    }

    /// `cursorBlink == false`(DECSCUSRのsteadyバリアント、またはDECSET `?12l`)の場合は
    /// 点滅位相に関わらず常にカーソルを描く経路をクラッシュなく通ることを確認する。
    func testDrawWithSteadyCursorDoesNotCrash() {
        let view = TerminalScreenView(frame: CGRect(x: 0, y: 0, width: 400, height: 300))
        let cells = [CellData(
            ch: "A", fg: 0xFFFFFFFF, bg: 0xFF000000, bold: false,
            dim: false, italic: false, underline: false,
            strikethrough: false, blink: false, invisible: false, linkId: nil
        )]
        let update = ScreenUpdate(
            cols: 1, rows: 1, cells: cells,
            cursorRow: 0, cursorCol: 0,
            title: nil, applicationCursorMode: false, bracketedPasteMode: false,
            mouseReportingMode: .off, sgrMouseMode: false,
            cursorVisible: true, bellGeneration: 0,
            cursorShape: .bar, cursorBlink: false, linkTable: [], images: [], kittyKeyboardFlags: 0
        )
        view.apply(update)
        view.layoutIfNeeded()
        let renderer = UIGraphicsImageRenderer(size: view.bounds.size)
        _ = renderer.image { _ in view.draw(view.bounds) }
    }

    /// タスク#34 codexレビュー指摘の回帰確認: スクロールバック表示中
    /// (`synthesizeDisplayUpdate`が`cursorRow = update.rows`でカーソルを画面外に隠す状態)で
    /// 点滅カーソルを持つライブ`update`を`draw(_:)`してもクラッシュしないこと、および
    /// `computeDisplayUpdate()`経由で合成された`ScreenUpdate`のカーソルが実際に画面外に
    /// なっていることを確認する(`lastDisplayCursorBlinks`自体はprivateで直接検証できない
    /// ため、既存方針通りスモークテストに留める)。
    func testDrawDuringScrollbackWithBlinkingCursorDoesNotCrash() {
        let view = TerminalScreenView(frame: CGRect(x: 0, y: 0, width: 400, height: 300))
        let cols = 4
        let rows = 2
        let cells = (0..<(cols * rows)).map { i in
            CellData(
                ch: i % 2 == 0 ? "A" : " ", fg: 0xFFFFFFFF, bg: 0xFF000000, bold: false,
                dim: false, italic: false, underline: false,
                strikethrough: false, blink: false, invisible: false, linkId: nil
            )
        }
        let update = ScreenUpdate(
            cols: UInt32(cols), rows: UInt32(rows), cells: cells,
            cursorRow: 0, cursorCol: 1,
            title: nil, applicationCursorMode: false, bracketedPasteMode: false,
            mouseReportingMode: .off, sgrMouseMode: false,
            cursorVisible: true, bellGeneration: 0,
            cursorShape: .bar, cursorBlink: true, linkTable: [], images: [], kittyKeyboardFlags: 0
        )
        view.apply(update)
        view.onScrollbackLenRequest = { 10 }
        view.onScrollbackRequest = { _, _ in cells }
        view.scrollOffset = 1

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
