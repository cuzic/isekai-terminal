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
            title: nil, applicationCursorMode: false, applicationKeypadMode: false, bracketedPasteMode: false,
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
            title: nil, applicationCursorMode: false, applicationKeypadMode: false, bracketedPasteMode: false,
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

    // MARK: - タスク#71: 空白セルのunderline/strikethrough

    /// `draw(_:)`は`guard !cell.ch.isEmpty, cell.ch != " " else { continue }`で
    /// 装飾判定より前に空白セルを早期スキップしていたため、`underline`/`strikethrough`
    /// が立っていても空白セルには何も描かれない不具合があった(Android版
    /// `SshTerminalCanvas.kt`の`hasLineDecoration`と同じ問題)。他のSGRテストは
    /// 「クラッシュしない」ことしか確認していないが、この不具合はクラッシュせず無音で
    /// 装飾だけが欠落するため、実際に`draw(_:)`が生成したビットマップの生ピクセルを
    /// 読み、装飾の有無でピクセルが変わることを直接検証する。修正前のコードでは
    /// `plainSpace == underlinedSpace`(どちらも早期continueで何も描かれない)となり
    /// このテストは失敗する。
    func testUnderlineAndStrikethroughOnBlankCellAffectRenderedPixels() {
        func renderedPixels(underline: Bool, strikethrough: Bool) -> [UInt8] {
            let view = TerminalScreenView(frame: CGRect(x: 0, y: 0, width: 40, height: 40))
            let cell = CellData(
                ch: " ", fg: 0xFFFFFFFF, bg: 0xFF000000, bold: false, dim: false, italic: false,
                underline: underline, strikethrough: strikethrough, blink: false, invisible: false,
                linkId: nil
            )
            let update = ScreenUpdate(
                cols: 1, rows: 1, cells: [cell],
                cursorRow: 0, cursorCol: 0,
                title: nil, applicationCursorMode: false, applicationKeypadMode: false,
                bracketedPasteMode: false,
                mouseReportingMode: .off, sgrMouseMode: false,
                cursorVisible: false, bellGeneration: 0,
                cursorShape: .block, cursorBlink: false, linkTable: [], images: [], kittyKeyboardFlags: 0
            )
            view.apply(update)
            view.layoutIfNeeded()
            let renderer = UIGraphicsImageRenderer(size: view.bounds.size)
            let image = renderer.image { _ in view.draw(view.bounds) }
            guard let cgImage = image.cgImage else {
                XCTFail("failed to obtain cgImage from rendered view")
                return []
            }
            let width = cgImage.width
            let height = cgImage.height
            var pixels = [UInt8](repeating: 0, count: width * height * 4)
            let colorSpace = CGColorSpaceCreateDeviceRGB()
            guard let context = CGContext(
                data: &pixels, width: width, height: height, bitsPerComponent: 8,
                bytesPerRow: width * 4, space: colorSpace,
                bitmapInfo: CGImageAlphaInfo.premultipliedLast.rawValue
            ) else {
                XCTFail("failed to create bitmap context for pixel inspection")
                return []
            }
            context.draw(cgImage, in: CGRect(x: 0, y: 0, width: width, height: height))
            return pixels
        }

        let plainSpace = renderedPixels(underline: false, strikethrough: false)
        let underlinedSpace = renderedPixels(underline: true, strikethrough: false)
        let strikethroughSpace = renderedPixels(underline: false, strikethrough: true)

        XCTAssertFalse(plainSpace.isEmpty)
        XCTAssertNotEqual(
            plainSpace, underlinedSpace,
            "underline付きの空白セルは装飾なしの空白セルと異なるピクセルになるはず(タスク#71)"
        )
        XCTAssertNotEqual(
            plainSpace, strikethroughSpace,
            "strikethrough付きの空白セルも装飾なしの空白セルと異なるピクセルになるはず(タスク#71)"
        )

        // このテストが「常にピクセルが変わる」という緩いテストに劣化していないことの
        // サニティチェック(同一入力なら描画結果は決定的であるべき)。
        let plainSpaceAgain = renderedPixels(underline: false, strikethrough: false)
        XCTAssertEqual(plainSpace, plainSpaceAgain, "同一入力の描画結果は決定的であるべき")
    }

    func testApplyIgnoresMismatchedCellCountWithoutCrashing() {
        let view = TerminalScreenView(frame: CGRect(x: 0, y: 0, width: 100, height: 100))
        let update = ScreenUpdate(
            cols: 10, rows: 10, cells: [],
            cursorRow: 0, cursorCol: 0,
            title: nil, applicationCursorMode: false, applicationKeypadMode: false, bracketedPasteMode: false,
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
                title: nil, applicationCursorMode: false, applicationKeypadMode: false, bracketedPasteMode: false,
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
            title: nil, applicationCursorMode: false, applicationKeypadMode: false, bracketedPasteMode: false,
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
            title: nil, applicationCursorMode: false, applicationKeypadMode: false, bracketedPasteMode: false,
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

    // MARK: - タスク#67: 検索バーの現在マッチハイライト(searchHighlight)

    /// `scrollOffset`が`searchHighlight.row`と一致する間はクラッシュせず描画できることの
    /// スモークテスト(実際のピクセル出力の目視確認は対象外、他の`testDraw*DoesNotCrash`と
    /// 同じ方針)。`col + len`が`cols`を超える(はみ出す)マッチでもクランプされ、
    /// はみ出し自体はクラッシュしないことも合わせて確認する。
    func testDrawWithSearchHighlightMatchingScrollOffsetDoesNotCrash() {
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
            title: nil, applicationCursorMode: false, applicationKeypadMode: false, bracketedPasteMode: false,
            mouseReportingMode: .off, sgrMouseMode: false,
            cursorVisible: true, bellGeneration: 0,
            cursorShape: .block, cursorBlink: false, linkTable: [], images: [], kittyKeyboardFlags: 0
        )
        view.apply(update)
        view.onScrollbackLenRequest = { 10 }
        view.onScrollbackRequest = { _, _ in cells }
        view.scrollOffset = 3
        // colsを超えてはみ出すマッチ(col + len > cols)——クランプされるだけでクラッシュしない
        // ことを確認する。
        view.searchHighlight = ScrollbackSearchMatch(row: 3, col: UInt32(cols - 1), len: 10)

        view.layoutIfNeeded()
        let renderer = UIGraphicsImageRenderer(size: view.bounds.size)
        _ = renderer.image { _ in view.draw(view.bounds) }
    }

    /// `scrollOffset`が`searchHighlight.row`と一致していない間(ジャンプ直後で
    /// まだ`scrollOffset`が届いていない・ライブ画面表示中等)はハイライトを描かない
    /// ガード自体がクラッシュしないことの確認。
    func testDrawWithSearchHighlightNotMatchingScrollOffsetDoesNotCrash() {
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
            title: nil, applicationCursorMode: false, applicationKeypadMode: false, bracketedPasteMode: false,
            mouseReportingMode: .off, sgrMouseMode: false,
            cursorVisible: true, bellGeneration: 0,
            cursorShape: .block, cursorBlink: true, linkTable: [], images: [], kittyKeyboardFlags: 0
        )
        view.apply(update)
        // scrollOffsetは既定の0(ライブ)のまま、row=5のマッチを持たせる(不一致)。
        view.searchHighlight = ScrollbackSearchMatch(row: 5, col: 0, len: 1)

        view.layoutIfNeeded()
        let renderer = UIGraphicsImageRenderer(size: view.bounds.size)
        _ = renderer.image { _ in view.draw(view.bounds) }
    }

    /// タスク#79: `scrollOffset == 0`のまま`showingScrollback`が真の場合(検索結果の
    /// scrollback最新行[row=0]へジャンプした状態)に`computeDisplayUpdate()`/`draw(_:)`が
    /// クラッシュしないことのスモークテスト(他の`testDraw*DoesNotCrash`と同じ方針、
    /// 実際のピクセル出力の目視確認は対象外)。この状態を実際に到達可能にした判断
    /// ロジック自体の回帰検出は`IsekaiTerminalCoreLogicTests/TerminalScreenSearchHighlightTests`
    /// (`searchHighlightMatch`、Linux上でも`swift test`可能なピュア関数)を参照。
    func testDrawWithSearchHighlightAtRowZeroWhileShowingScrollbackDoesNotCrash() {
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
            title: nil, applicationCursorMode: false, applicationKeypadMode: false, bracketedPasteMode: false,
            mouseReportingMode: .off, sgrMouseMode: false,
            cursorVisible: true, bellGeneration: 0,
            cursorShape: .block, cursorBlink: false, linkTable: [], images: [], kittyKeyboardFlags: 0
        )
        view.apply(update)
        view.onScrollbackLenRequest = { 10 }
        view.onScrollbackRequest = { _, _ in cells }
        // scrollOffsetは既定の0(ライブと数値上は同じ)のまま、showingScrollbackだけを
        // 真にする——これがタスク#79で新しく到達可能になった状態。
        view.showingScrollback = true
        view.searchHighlight = ScrollbackSearchMatch(row: 0, col: 0, len: 1)

        view.layoutIfNeeded()
        let renderer = UIGraphicsImageRenderer(size: view.bounds.size)
        _ = renderer.image { _ in view.draw(view.bounds) }
    }

    /// `scrollOffset == 0`かつ`showingScrollback == false`(通常のライブ画面表示)の間、
    /// row=0のマッチがあっても`draw(_:)`のガードがクラッシュしないことのスモークテスト
    /// (判断ロジック自体の回帰検出は`TerminalScreenSearchHighlightTests`参照、上と同じ方針)。
    func testDrawWithSearchHighlightAtRowZeroWhileLiveDoesNotCrash() {
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
            title: nil, applicationCursorMode: false, applicationKeypadMode: false, bracketedPasteMode: false,
            mouseReportingMode: .off, sgrMouseMode: false,
            cursorVisible: true, bellGeneration: 0,
            cursorShape: .block, cursorBlink: true, linkTable: [], images: [], kittyKeyboardFlags: 0
        )
        view.apply(update)
        view.searchHighlight = ScrollbackSearchMatch(row: 0, col: 0, len: 1)

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

    // MARK: - タスク#81: wheelEvents(トラックパッド/マウスホイールのwheel up/down送出)
    //
    // codexレビュー(グループD)指摘: iOS側のマウスレポーティング配線は`UITouch`を常に
    // `.left`ボタンとして送るだけで、Android版`PointerEventType.Scroll`に相当する
    // ホイール/トラックパッドスクロールの`WheelUp`/`WheelDown`送信経路が無かった。
    // `handlePan`自体(`UIPanGestureRecognizer`依存)はテストしづらいため、`clampedFontScale`
    // と同様にUIKit非依存の純粋関数へロジックを切り出してここで直接検証する。

    func testWheelEventsAccumulatesBelowThresholdWithoutFiring() {
        let result = wheelEvents(deltaY: 4, carry: 0, cellHeight: 10)
        XCTAssertTrue(result.buttons.isEmpty)
        XCTAssertEqual(result.carry, 4, accuracy: 0.0001)
    }

    func testWheelEventsFiresWheelUpForNegativeDeltaPastThreshold() {
        // 負方向(コンテンツが上へ動く=履歴を遡る)は`scrollOffset`を増やす操作と同じ向き
        // なので、xtermの"wheel up"(button 64)に対応する。
        let result = wheelEvents(deltaY: -12, carry: 0, cellHeight: 10)
        XCTAssertEqual(result.buttons, [.wheelUp])
        XCTAssertEqual(result.carry, -2, accuracy: 0.0001)
    }

    func testWheelEventsFiresWheelDownForPositiveDeltaPastThreshold() {
        let result = wheelEvents(deltaY: 12, carry: 0, cellHeight: 10)
        XCTAssertEqual(result.buttons, [.wheelDown])
        XCTAssertEqual(result.carry, 2, accuracy: 0.0001)
    }

    func testWheelEventsFiresMultipleEventsForLargeDelta() {
        // トラックパッドの速いフリックのように、1回の`.changed`で複数セル分のtranslationが
        // 一度に届く場合でも、セル数分のwheelイベントに分割して送る(1notchずつ)。
        let result = wheelEvents(deltaY: -35, carry: 0, cellHeight: 10)
        XCTAssertEqual(result.buttons, [.wheelUp, .wheelUp, .wheelUp])
        XCTAssertEqual(result.carry, -5, accuracy: 0.0001)
    }

    func testWheelEventsCarriesFractionalAccumulationAcrossCalls() {
        // 小刻みなtranslation(トラックパッドの連続scroll)が複数回の`.changed`にまたがって
        // 届いても、`carry`を次呼び出しへ持ち越すことで合計が閾値を超えた時点で発火する。
        let first = wheelEvents(deltaY: -6, carry: 0, cellHeight: 10)
        XCTAssertTrue(first.buttons.isEmpty)
        let second = wheelEvents(deltaY: -6, carry: first.carry, cellHeight: 10)
        XCTAssertEqual(second.buttons, [.wheelUp])
        XCTAssertEqual(second.carry, -2, accuracy: 0.0001)
    }

    func testWheelEventsWithZeroCellHeightReturnsEmptyWithoutCrashing() {
        // フォントメトリクス確定前(初回layout前)に間接scrollが届いても0除算/無限ループに
        // ならないことの回帰確認。
        let result = wheelEvents(deltaY: -50, carry: 3, cellHeight: 0)
        XCTAssertTrue(result.buttons.isEmpty)
        XCTAssertEqual(result.carry, 3, accuracy: 0.0001)
    }

    // MARK: - タスク#87: マウスUI裁定ロジック(mouseReportingActive/decideMouseTouchBeganAction)
    //
    // fableレビュー(グループD)指摘: マウスレポーティングのpress/drag/releaseライフサイクル・
    // 2本指中断・scrollOffsetゲートの裁定ロジックに単体テストが無かった。`clampedFontScale`/
    // `wheelEvents`と同様、UIKit非依存の純粋関数へ抽出した上で直接検証する。

    func testMouseReportingActiveWhenModeIsNotOffAndLiveAndNotShowingScrollback() {
        XCTAssertTrue(mouseReportingActive(
            scrollOffset: 0, showingScrollback: false, mouseReportingMode: .normal
        ))
    }

    func testMouseReportingInactiveWhenModeIsOff() {
        XCTAssertFalse(mouseReportingActive(
            scrollOffset: 0, showingScrollback: false, mouseReportingMode: .off
        ))
    }

    func testMouseReportingInactiveWhenScrolledIntoScrollback() {
        // 過去ログを表示中にライブ側のモードへ従ってポインタイベントを送ると、
        // 表示対象(スクロールバック)と入力対象(ライブセッション)が食い違う。
        XCTAssertFalse(mouseReportingActive(
            scrollOffset: 3, showingScrollback: false, mouseReportingMode: .normal
        ))
    }

    func testMouseReportingInactiveWhileShowingScrollbackEvenIfScrollOffsetIsZero() {
        // タスク#79: 検索ジャンプでscrollback最新行(row=0)を表示中は、
        // scrollOffset == 0のままでもライブ表示ではない。
        XCTAssertFalse(mouseReportingActive(
            scrollOffset: 0, showingScrollback: true, mouseReportingMode: .normal
        ))
    }

    func testDecideMouseTouchBeganActionStartsTrackingForFirstSingleFingerTouch() {
        XCTAssertEqual(
            decideMouseTouchBeganAction(hasActiveTrackedTouch: false, totalTouchCount: 1),
            .startTracking
        )
    }

    func testDecideMouseTouchBeganActionReleasesActiveWhenASecondFingerTouchesDown() {
        // 追跡中のタッチがある間に2本目以降の指が触れた場合、直前のpressに対応する
        // releaseを送って打ち切る(releaseを送らないとリモート側でボタンが
        // 押されっぱなしに見える、Android版`decideMouseTouchStep`のRELEASE_AND_HANDOFF_TO_PINCHと
        // 同じ理由でトリガーされる)。
        XCTAssertEqual(
            decideMouseTouchBeganAction(hasActiveTrackedTouch: true, totalTouchCount: 2),
            .releaseActiveAndStopTracking
        )
    }

    func testDecideMouseTouchBeganActionReleasesActiveRegardlessOfTotalTouchCount() {
        // 追跡中のタッチがある限り、totalTouchCountの値に関わらず(既に離れて0になって
        // いても)releaseして打ち切る(元実装の`if let active = activeMouseTouch { ... }`が
        // totalTouchCountを見ずに先に判定していたのと対称)。
        XCTAssertEqual(
            decideMouseTouchBeganAction(hasActiveTrackedTouch: true, totalTouchCount: 1),
            .releaseActiveAndStopTracking
        )
    }

    func testDecideMouseTouchBeganActionIgnoresWhenNoActiveTouchAndAlreadyMultiFinger() {
        // 追跡中のタッチが無い状態でこの`touchesBegan`自体が最初から複数指として
        // 発火した場合は、マウスタッチとしては追跡を開始しない(pinch等に譲る)。
        XCTAssertEqual(
            decideMouseTouchBeganAction(hasActiveTrackedTouch: false, totalTouchCount: 2),
            .ignore
        )
    }

    // MARK: - タスク#88: shouldReportMouseMotion(ドラッグ中のセル単位dedup)

    func testShouldReportMouseMotionFalseWhenCellUnchanged() {
        // xtermは同一セル内でのマウス移動を重複報告しない。
        XCTAssertFalse(shouldReportMouseMotion(
            lastReportedCell: CellPos(row: 3, col: 5), newCell: CellPos(row: 3, col: 5)
        ))
    }

    func testShouldReportMouseMotionTrueWhenRowChanges() {
        XCTAssertTrue(shouldReportMouseMotion(
            lastReportedCell: CellPos(row: 3, col: 5), newCell: CellPos(row: 4, col: 5)
        ))
    }

    func testShouldReportMouseMotionTrueWhenColChanges() {
        XCTAssertTrue(shouldReportMouseMotion(
            lastReportedCell: CellPos(row: 3, col: 5), newCell: CellPos(row: 3, col: 6)
        ))
    }

    /// codexレビュー指摘: タスク#88の再現条件そのもの——`touchesMoved`が
    /// `lastMotionCell`を更新しながら抑止する逐次処理をここで模倣し、120Hz相当で
    /// 同じセル内へ複数回飛んできたMOTIONが1回も送信されず、実際にセルが変わった
    /// 時だけ送信されることを検証する(Android版
    /// `MouseGestureArbiterTest.testABurstOfSameCellMotionEventsAfterPressCollapsesToASingleReport`
    /// と対称)。
    func testShouldReportMouseMotionCollapsesABurstOfSameCellEventsAfterPress() {
        let pressCell = CellPos(row: 3, col: 5)
        let incomingMotionEvents = [
            CellPos(row: 3, col: 5), CellPos(row: 3, col: 5), CellPos(row: 3, col: 5),
            CellPos(row: 4, col: 5),
            CellPos(row: 4, col: 5), CellPos(row: 4, col: 5),
        ]
        var lastMotionCell = pressCell
        var reportedCells: [CellPos] = []
        for cell in incomingMotionEvents {
            if shouldReportMouseMotion(lastReportedCell: lastMotionCell, newCell: cell) {
                lastMotionCell = cell
                reportedCells.append(cell)
            }
        }
        XCTAssertEqual(reportedCells, [CellPos(row: 4, col: 5)])
    }

    // MARK: - タスク#86: shouldResetBlinkPhase(blink初期表示位相の安定化)

    /// blink無し→blink有りへ新規遷移した場合はリセットが必要(SGR blinkセルの出現)。
    func testShouldResetBlinkPhaseWhenBlinkCellFirstAppears() {
        XCTAssertTrue(shouldResetBlinkPhase(
            newHasBlink: true, newCursorBlinks: false,
            previousHasBlink: false, previousCursorBlinks: false
        ))
    }

    /// blink無し→blink有りへ新規遷移した場合はリセットが必要(点滅カーソルの出現)。
    func testShouldResetBlinkPhaseWhenCursorBlinkFirstAppears() {
        XCTAssertTrue(shouldResetBlinkPhase(
            newHasBlink: false, newCursorBlinks: true,
            previousHasBlink: false, previousCursorBlinks: false
        ))
    }

    /// 既にSGR blinkセルが表示されている状態で点滅カーソルが追加されても、
    /// (どちらか一方は既に有った以上)新規遷移ではないためリセット不要
    /// ——位相が既に「点灯」側にあるとは限らないが、その位相自体は既存の
    /// blinkセルにとっては正しく継続中であるべきで、勝手に巻き戻さない。
    func testShouldResetBlinkPhaseNotNeededWhenAlreadyBlinkingBeforehand() {
        XCTAssertFalse(shouldResetBlinkPhase(
            newHasBlink: true, newCursorBlinks: true,
            previousHasBlink: true, previousCursorBlinks: false
        ))
    }

    /// blink有りの状態が継続しているだけ(前回も今回もSGR blinkセルが有る)ならリセット不要。
    func testShouldResetBlinkPhaseNotNeededWhenBlinkContinues() {
        XCTAssertFalse(shouldResetBlinkPhase(
            newHasBlink: true, newCursorBlinks: false,
            previousHasBlink: true, previousCursorBlinks: false
        ))
    }

    /// blinkが無い状態が続いているだけならリセット不要(トグルは走っていても無関係)。
    func testShouldResetBlinkPhaseNotNeededWhenNoBlinkAtAll() {
        XCTAssertFalse(shouldResetBlinkPhase(
            newHasBlink: false, newCursorBlinks: false,
            previousHasBlink: false, previousCursorBlinks: false
        ))
    }

    /// blink有り→blink無しへ遷移する場合もリセット不要(新規出現ではなく消滅なので、
    /// 次に何か出現するまで位相を動かす理由が無い)。
    func testShouldResetBlinkPhaseNotNeededWhenBlinkDisappears() {
        XCTAssertFalse(shouldResetBlinkPhase(
            newHasBlink: false, newCursorBlinks: false,
            previousHasBlink: true, previousCursorBlinks: false
        ))
    }

    /// `draw(_:)`自体の回帰確認: blink無し→blink有りへ新規遷移する`ScreenUpdate`の連続適用が
    /// クラッシュせず完走すること(他の`testDraw*DoesNotCrash`と同じ方針。`blinkPhaseVisible`は
    /// privateで直接検証できないため、実際のリセットはユニットテスト側の
    /// `shouldResetBlinkPhase`で検証する)。
    func testDrawAfterBlinkCellNewlyAppearsDoesNotCrash() {
        let view = TerminalScreenView(frame: CGRect(x: 0, y: 0, width: 400, height: 300))
        func cell(blink: Bool) -> CellData {
            CellData(
                ch: "A", fg: 0xFFFFFFFF, bg: 0xFF000000, bold: false, dim: false, italic: false,
                underline: false, strikethrough: false, blink: blink, invisible: false, linkId: nil
            )
        }
        func update(blink: Bool) -> ScreenUpdate {
            ScreenUpdate(
                cols: 1, rows: 1, cells: [cell(blink: blink)],
                cursorRow: 0, cursorCol: 0,
                title: nil, applicationCursorMode: false, applicationKeypadMode: false, bracketedPasteMode: false,
                mouseReportingMode: .off, sgrMouseMode: false,
                cursorVisible: false, bellGeneration: 0,
                cursorShape: .block, cursorBlink: false, linkTable: [], images: [], kittyKeyboardFlags: 0
            )
        }

        view.apply(update(blink: false))
        view.layoutIfNeeded()
        let renderer = UIGraphicsImageRenderer(size: view.bounds.size)
        _ = renderer.image { _ in view.draw(view.bounds) }

        // blink無し→blink有りへ新規遷移。
        view.apply(update(blink: true))
        _ = renderer.image { _ in view.draw(view.bounds) }
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

    // MARK: - タスク#89: SixelBitmapCache

    /// Android版`SshTerminalCanvasTest.kt`(`SixelBitmapCache decodes a bitmap for each
    /// distinct id`)と対称。1x1 RGBAの`ImagePlacement`を作る共通ヘルパー。
    private func sixelImagePlacement(id: UInt64, width: Int = 1, height: Int = 1, rgbaByte: UInt8 = 0xFF) -> ImagePlacement {
        ImagePlacement(
            id: id, row: 0, col: 0, rowsSpan: 1, colsSpan: 1,
            widthPx: UInt32(width), heightPx: UInt32(height),
            rgba: Data(repeating: rgbaByte, count: width * height * 4)
        )
    }

    /// `image`の左上ピクセルをARGB8888(`0xAARRGGBB`)として読み出す(既存の
    /// `testUnderlineAndStrikethroughOnBlankCellAffectRenderedPixels`と同じ
    /// `CGContext`読み出し手法)。
    private func topLeftPixelArgb(of image: UIImage) -> UInt32? {
        guard let cgImage = image.cgImage else { return nil }
        var pixel: [UInt8] = [0, 0, 0, 0]
        let colorSpace = CGColorSpaceCreateDeviceRGB()
        guard let context = CGContext(
            data: &pixel, width: 1, height: 1, bitsPerComponent: 8,
            bytesPerRow: 4, space: colorSpace,
            bitmapInfo: CGImageAlphaInfo.premultipliedLast.rawValue
        ) else { return nil }
        context.draw(cgImage, in: CGRect(x: 0, y: 0, width: 1, height: 1))
        let r = UInt32(pixel[0])
        let g = UInt32(pixel[1])
        let b = UInt32(pixel[2])
        let a = UInt32(pixel[3])
        return (a << 24) | (r << 16) | (g << 8) | b
    }

    func testSixelBitmapCacheDecodesAnImageForEachDistinctId() {
        let cache = SixelBitmapCache()
        let first = cache.image(for: sixelImagePlacement(id: 1))
        let second = cache.image(for: sixelImagePlacement(id: 2))
        XCTAssertNotNil(first)
        XCTAssertNotNil(second)
    }

    func testSixelBitmapCacheReusesSameImageInstanceForIdSeenAgain() {
        let cache = SixelBitmapCache()
        let placement = sixelImagePlacement(id: 1)
        let first = cache.image(for: placement)
        let second = cache.image(for: placement)
        XCTAssertNotNil(first)
        XCTAssertTrue(first === second, "同じidなら再デコードせず同一UIImageインスタンスを返すこと")
    }

    /// Android版`SixelBitmapCache drops entries whose id is no longer live`と対称。
    /// `prune(liveIds:)`後に同じidを再度`image(for:)`すると、キャッシュから捨てられた分
    /// 新たにデコードし直された(＝以前と異なるインスタンスの)`UIImage`が返ることで
    /// 「捨てられたこと」を間接的に確認する(このクラスは内部辞書を公開しないため)。
    func testSixelBitmapCacheDropsEntriesWhoseIdIsNoLongerLive() {
        let cache = SixelBitmapCache()
        let placement1 = sixelImagePlacement(id: 1)
        let placement2 = sixelImagePlacement(id: 2)
        let firstImageForId1 = cache.image(for: placement1)
        _ = cache.image(for: placement2)

        // idが2のものだけが「生きている」とみなし、id=1はキャッシュから捨てられるはず。
        cache.prune(liveIds: [2])

        let secondImageForId1 = cache.image(for: placement1)
        XCTAssertNotNil(firstImageForId1)
        XCTAssertNotNil(secondImageForId1)
        XCTAssertFalse(
            firstImageForId1 === secondImageForId1,
            "liveIdsに含まれないidはpruneでキャッシュから捨てられ、再度image(for:)すると新規デコードされること"
        )
    }

    /// Android版`SixelBitmapCache decodes red and blue pixels without channel swap`と対称。
    /// `sixel.rs`が詰めるRGBA8888バイト順から作った`CGImage`が(`premultipliedLast`解釈で)
    /// 赤/青チャンネルを入れ替えずに描画できることを確認する。
    func testSixelBitmapCacheDecodesRedAndBluePixelsWithoutChannelSwap() {
        let cache = SixelBitmapCache()
        // R=0xFF,G=0x00,B=0x00,A=0xFF(赤、不透明)
        let red = ImagePlacement(
            id: 1, row: 0, col: 0, rowsSpan: 1, colsSpan: 1,
            widthPx: 1, heightPx: 1,
            rgba: Data([0xFF, 0x00, 0x00, 0xFF])
        )
        // R=0x00,G=0x00,B=0xFF,A=0xFF(青、不透明)
        let blue = ImagePlacement(
            id: 2, row: 0, col: 0, rowsSpan: 1, colsSpan: 1,
            widthPx: 1, heightPx: 1,
            rgba: Data([0x00, 0x00, 0xFF, 0xFF])
        )

        guard let redImage = cache.image(for: red), let blueImage = cache.image(for: blue) else {
            XCTFail("expected both placements to decode")
            return
        }
        XCTAssertEqual(topLeftPixelArgb(of: redImage), 0xFFFF0000)
        XCTAssertEqual(topLeftPixelArgb(of: blueImage), 0xFF0000FF)
    }

    /// `decode(_:)`の`width * height * 4`境界チェック(codexレビュー指摘、Android版
    /// `SixelBitmapCache.isSane`と対称)。寸法とバッファ長が矛盾する`ImagePlacement`は
    /// クラッシュせず`nil`を返すこと。
    func testSixelBitmapCacheRejectsMismatchedBufferLength() {
        let cache = SixelBitmapCache()
        let malformed = ImagePlacement(
            id: 1, row: 0, col: 0, rowsSpan: 1, colsSpan: 1,
            widthPx: 4, heightPx: 4,
            rgba: Data(repeating: 0xFF, count: 10) // 4*4*4 = 64バイト必要なのに10バイトしかない
        )
        XCTAssertNil(cache.image(for: malformed))
    }

    /// 幅・高さのいずれかが0の`ImagePlacement`はクラッシュせず`nil`を返すこと
    /// (Android版`isSane`の`w <= 0 || h <= 0`ガードと対称)。
    func testSixelBitmapCacheRejectsZeroDimensions() {
        let cache = SixelBitmapCache()
        let zeroWidth = ImagePlacement(
            id: 1, row: 0, col: 0, rowsSpan: 1, colsSpan: 1,
            widthPx: 0, heightPx: 4, rgba: Data()
        )
        XCTAssertNil(cache.image(for: zeroWidth))
    }

    /// `MAX_SIXEL_DIM`(4096)を超える単一辺は、バッファ長自体は矛盾していなくても
    /// 拒否されること(Android版`isSane`の`w > 4096 || h > 4096`ガードと対称、
    /// codexレビュー指摘)。
    func testSixelBitmapCacheRejectsDimensionExceedingMaxDimension() {
        let cache = SixelBitmapCache()
        let width = 5000
        let height = 1
        let tooWide = ImagePlacement(
            id: 1, row: 0, col: 0, rowsSpan: 1, colsSpan: 1,
            widthPx: UInt32(width), heightPx: UInt32(height),
            rgba: Data(repeating: 0xFF, count: width * height * 4)
        )
        XCTAssertNil(cache.image(for: tooWide))
    }

    /// 各辺は`MAX_SIXEL_DIM`以下でも、面積が`MAX_SIXEL_AREA`(4,000,000)を超えれば
    /// 拒否されること(Android版`isSane`の`w.toLong() * h.toLong() > 4_000_000L`
    /// ガードと対称)。
    func testSixelBitmapCacheRejectsAreaExceedingMaxArea() {
        let cache = SixelBitmapCache()
        let width = 2001
        let height = 2000 // 面積 4,002,000 > 4,000,000(各辺は4096以下)
        let tooLarge = ImagePlacement(
            id: 1, row: 0, col: 0, rowsSpan: 1, colsSpan: 1,
            widthPx: UInt32(width), heightPx: UInt32(height),
            rgba: Data(repeating: 0xFF, count: width * height * 4)
        )
        XCTAssertNil(cache.image(for: tooLarge))
    }
}
