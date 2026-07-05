import UIKit
import TsshCoreLogic

/// Phase 1F-2(#49): ピンチズームでのフォント拡縮率のクランプ計算(0.5〜3.0)。
/// Android版`fontScale.coerceIn(0.5f, 3.0f)`と対称。UIKitのジェスチャコールバックから
/// 分離してあるためテスト容易(ネットワーク/UIに触れない純粋関数)。
func clampedFontScale(current: CGFloat, zoomDelta: CGFloat) -> CGFloat {
    min(max(current * zoomDelta, 0.5), 3.0)
}

/// Phase 1D: ターミナル本画面の描画。Rust→Kotlin間で既に使われている
/// `ScreenUpdate`/`CellData`(ARGBパックの32bit色)を直接消費する
/// (Phase 1A-6の`TerminalFrameBatch`/`PackedRow`は診断用の並行表現であり、
/// 実際のレンダリング統合では使わないというPLAN.md記載の方針に従う)。
public final class TerminalScreenView: UIView {
    private var latestUpdate: ScreenUpdate?
    private static let baseFontSize: CGFloat = 14
    private var font = UIFont.monospacedSystemFont(ofSize: baseFontSize, weight: .regular)
    private var boldFont = UIFont.monospacedSystemFont(ofSize: baseFontSize, weight: .bold)
    private var cellSize: CGSize = .zero

    /// Phase 1F-1(#48): 現在の選択範囲(行単位)。Android版`SelectionRange`と対称。
    /// 非nilの間`draw(_:)`でハイライトを描画する。
    public var selection: SelectionRange? {
        didSet { setNeedsDisplay() }
    }
    /// 選択範囲が変化する度に呼ばれる(SwiftUI側のフローティングツールバー表示に使う)。
    public var onSelectionChanged: ((SelectionRange?) -> Void)?

    /// Phase 1F-2(#49): フォントサイズの拡縮率(Android版`fontScale`、0.5〜3.0に
    /// クランプ、既定1.0)。SwiftUI側で`UserDefaults`(キー`"font_scale"`、Android版
    /// `SharedPreferences`の`"font_scale"`キーと対称)へ永続化する。
    public var fontScale: CGFloat = 1.0 {
        didSet {
            guard fontScale != oldValue else { return }
            updateFontMetrics()
            setNeedsDisplay()
        }
    }
    /// ピンチジェスチャで拡縮率が変化する度に呼ばれる(SwiftUI側での永続化に使う)。
    public var onFontScaleChanged: ((CGFloat) -> Void)?

    /// Phase 1F-4(#51): スクロールバックのスワイプで表示中のオフセット(0 = ライブ)。
    /// Android版`scrollOffset`と対称。SwiftUI側の「ライブへ戻る」ボタンからも
    /// (`selection`/`fontScale`と同様の双方向バインディングで)0を書き戻せる。
    public var scrollOffset: UInt32 = 0 {
        didSet {
            guard scrollOffset != oldValue else { return }
            if scrollOffset == 0 { panAccumY = 0 }
            onScrollOffsetChanged?(scrollOffset)
            setNeedsDisplay()
        }
    }
    /// スクロールバックの行を取得するクロージャ(Android版`actions.onScrollbackCells`相当)。
    public var onScrollbackRequest: ((_ offset: UInt32, _ rows: UInt32) -> [CellData])?
    /// スクロールバックの総行数を取得するクロージャ(Android版`uiState.scrollbackLen`相当)。
    public var onScrollbackLenRequest: (() -> UInt32)?
    /// スクロールオフセットが変化する度に呼ばれる(SwiftUI側の状態同期に使う)。
    public var onScrollOffsetChanged: ((UInt32) -> Void)?
    private var panAccumY: CGFloat = 0

    public override init(frame: CGRect) {
        super.init(frame: frame)
        backgroundColor = .black
        contentMode = .redraw
        isOpaque = true
        updateFontMetrics()

        let longPress = UILongPressGestureRecognizer(target: self, action: #selector(handleLongPress(_:)))
        longPress.minimumPressDuration = 0.4
        addGestureRecognizer(longPress)

        let pinch = UIPinchGestureRecognizer(target: self, action: #selector(handlePinch(_:)))
        addGestureRecognizer(pinch)

        let pan = UIPanGestureRecognizer(target: self, action: #selector(handlePan(_:)))
        pan.maximumNumberOfTouches = 1
        addGestureRecognizer(pan)
    }

    public required init?(coder: NSCoder) {
        super.init(coder: coder)
    }

    /// 最新の画面状態を反映する。`MainActor`から呼ぶこと。
    public func apply(_ update: ScreenUpdate) {
        latestUpdate = update
        setNeedsDisplay()
    }

    private func updateFontMetrics() {
        let size = Self.baseFontSize * fontScale
        font = UIFont.monospacedSystemFont(ofSize: size, weight: .regular)
        boldFont = UIFont.monospacedSystemFont(ofSize: size, weight: .bold)
        let measured = ("M" as NSString).size(withAttributes: [.font: font])
        cellSize = CGSize(width: measured.width, height: font.lineHeight)
    }

    /// ピンチズームでのフォントサイズ調整(Android版`TerminalScreen.kt`の
    /// `event.calculateZoom()`+`fontScale.coerceIn(0.5f, 3.0f)`と対称)。
    @objc private func handlePinch(_ recognizer: UIPinchGestureRecognizer) {
        guard recognizer.state == .changed else { return }
        let newScale = clampedFontScale(current: fontScale, zoomDelta: recognizer.scale)
        recognizer.scale = 1.0
        guard newScale != fontScale else { return }
        fontScale = newScale
        onFontScaleChanged?(newScale)
    }

    /// スクロールバックのスワイプ(Android版`TerminalScreen.kt`の`panAccumY`+
    /// `event.calculatePan()`ループと対称)。縦方向のドラッグ量を蓄積し、セル1行分
    /// 溜まる度に`scrollOffset`を1ずつ増減する。長押し(選択)が既に認識されている間は
    /// UIKitの既定動作(同一ビュー上の複数ジェスチャの同時認識は既定でOFF)により
    /// このpanは発火しない。
    @objc private func handlePan(_ recognizer: UIPanGestureRecognizer) {
        guard recognizer.state == .changed, cellSize.height > 0 else { return }
        let translation = recognizer.translation(in: self)
        recognizer.setTranslation(.zero, in: self)
        panAccumY += translation.y

        let scrollbackLen = onScrollbackLenRequest?() ?? 0
        let cellHeight = cellSize.height
        while panAccumY < -cellHeight {
            if scrollOffset < scrollbackLen { scrollOffset += 1 }
            panAccumY += cellHeight
        }
        while panAccumY > cellHeight {
            if scrollOffset > 0 { scrollOffset -= 1 }
            panAccumY -= cellHeight
        }
    }

    /// `scrollOffset`が0ならライブの`latestUpdate`をそのまま、それ以外は
    /// `onScrollbackRequest`でスクロールバックの行を取得してカーソルを画面外に隠した
    /// `ScreenUpdate`を合成する(Android版`displayUpdate`の`remember(scrollOffset, ...)`と
    /// 同じ役割)。
    private func computeDisplayUpdate() -> ScreenUpdate? {
        guard let update = latestUpdate else { return nil }
        guard scrollOffset > 0 else { return update }
        let cells = onScrollbackRequest?(scrollOffset, update.rows) ?? []
        return synthesizeDisplayUpdate(live: update, scrollOffset: scrollOffset, scrollbackCells: cells)
    }

    /// 長押し+ドラッグでの行単位テキスト選択(Android版`TerminalScreen.kt`の
    /// `awaitLongPressOrCancellation`+ドラッグループと対称)。`UILongPressGestureRecognizer`は
    /// `.began`後の移動でも認識状態を維持し続けて`.changed`を報告し続けるため、
    /// 別途pan gestureを組み合わせる必要はない。
    @objc private func handleLongPress(_ recognizer: UILongPressGestureRecognizer) {
        guard let update = computeDisplayUpdate() else { return }
        let cols = Int(update.cols)
        let rows = Int(update.rows)
        let point = recognizer.location(in: self)
        let cell = offsetToCellPos(x: Double(point.x), y: Double(point.y), cellWidth: Double(cellSize.width), cellHeight: Double(cellSize.height), cols: cols, rows: rows)

        switch recognizer.state {
        case .began:
            let newSelection = SelectionRange(anchor: cell, head: cell)
            selection = newSelection
            onSelectionChanged?(newSelection)
        case .changed:
            guard var current = selection else { return }
            current.head = cell
            selection = current
            onSelectionChanged?(current)
        default:
            break
        }
    }

    public override func draw(_ rect: CGRect) {
        guard let update = computeDisplayUpdate() else { return }
        let cols = Int(update.cols)
        let rows = Int(update.rows)
        guard cols > 0, rows > 0, update.cells.count == cols * rows else { return }

        let cellWidth = cellSize.width
        let cellHeight = cellSize.height

        for row in 0..<rows {
            for col in 0..<cols {
                let cell = update.cells[row * cols + col]
                let x = CGFloat(col) * cellWidth
                let y = CGFloat(row) * cellHeight
                let cellRect = CGRect(x: x, y: y, width: cellWidth, height: cellHeight)

                let bg = Self.colorFromPackedArgb(cell.bg)
                bg.setFill()
                UIRectFill(cellRect)

                guard !cell.ch.isEmpty, cell.ch != " " else { continue }
                let fg = Self.colorFromPackedArgb(cell.fg)
                let attrs: [NSAttributedString.Key: Any] = [
                    .font: cell.bold ? boldFont : font,
                    .foregroundColor: fg,
                ]
                (cell.ch as NSString).draw(at: CGPoint(x: x, y: y), withAttributes: attrs)
            }
        }

        // 選択範囲のハイライト(行単位)。Android版`SshTerminalCanvas.kt`はセル背景の
        // 前(下)に半透明色を敷くが、iOS版は各セルの背景を無条件に不透明で塗るため
        // (上のループ参照)、ここでは代わりにセル描画の後にオーバーレイとして重ねる。
        if let selection {
            let startRow = min(max(selection.startRow, 0), rows - 1)
            let endRow = min(max(selection.endRow, 0), rows - 1)
            if startRow <= endRow {
                UIColor.white.withAlphaComponent(120.0 / 255.0).setFill()
                for row in startRow...endRow {
                    let y = CGFloat(row) * cellHeight
                    UIRectFill(CGRect(x: 0, y: y, width: CGFloat(cols) * cellWidth, height: cellHeight))
                }
            }
        }

        if Int(update.cursorRow) < rows, Int(update.cursorCol) < cols {
            let x = CGFloat(update.cursorCol) * cellWidth
            let y = CGFloat(update.cursorRow) * cellHeight
            UIColor.white.withAlphaComponent(0.5).setFill()
            UIRectFill(CGRect(x: x, y: y, width: cellWidth, height: cellHeight))
        }
    }

    /// Android版`CellData.fg`/`bg`と同じARGBパック形式(0xAARRGGBB)として解釈する
    /// (`ui/SshTerminalCanvas.kt`が`cell.bg.toInt()`をAndroidの`Color` intとして
    /// そのまま使っているのと対称)。
    private static func colorFromPackedArgb(_ value: UInt32) -> UIColor {
        let a = CGFloat((value >> 24) & 0xFF) / 255.0
        let r = CGFloat((value >> 16) & 0xFF) / 255.0
        let g = CGFloat((value >> 8) & 0xFF) / 255.0
        let b = CGFloat(value & 0xFF) / 255.0
        return UIColor(red: r, green: g, blue: b, alpha: a == 0 ? 1.0 : a)
    }
}
