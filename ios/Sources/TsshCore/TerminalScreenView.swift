import UIKit

/// Phase 1D: ターミナル本画面の描画。Rust→Kotlin間で既に使われている
/// `ScreenUpdate`/`CellData`(ARGBパックの32bit色)を直接消費する
/// (Phase 1A-6の`TerminalFrameBatch`/`PackedRow`は診断用の並行表現であり、
/// 実際のレンダリング統合では使わないというPLAN.md記載の方針に従う)。
public final class TerminalScreenView: UIView {
    private var latestUpdate: ScreenUpdate?
    private let font = UIFont.monospacedSystemFont(ofSize: 14, weight: .regular)
    private lazy var boldFont = UIFont.monospacedSystemFont(ofSize: 14, weight: .bold)
    private lazy var cellSize: CGSize = {
        let size = ("M" as NSString).size(withAttributes: [.font: font])
        return CGSize(width: size.width, height: font.lineHeight)
    }()

    /// Phase 1F-1(#48): 現在の選択範囲(行単位)。Android版`SelectionRange`と対称。
    /// 非nilの間`draw(_:)`でハイライトを描画する。
    public var selection: SelectionRange? {
        didSet { setNeedsDisplay() }
    }
    /// 選択範囲が変化する度に呼ばれる(SwiftUI側のフローティングツールバー表示に使う)。
    public var onSelectionChanged: ((SelectionRange?) -> Void)?

    public override init(frame: CGRect) {
        super.init(frame: frame)
        backgroundColor = .black
        contentMode = .redraw
        isOpaque = true

        let longPress = UILongPressGestureRecognizer(target: self, action: #selector(handleLongPress(_:)))
        longPress.minimumPressDuration = 0.4
        addGestureRecognizer(longPress)
    }

    public required init?(coder: NSCoder) {
        super.init(coder: coder)
    }

    /// 最新の画面状態を反映する。`MainActor`から呼ぶこと。
    public func apply(_ update: ScreenUpdate) {
        latestUpdate = update
        setNeedsDisplay()
    }

    /// 長押し+ドラッグでの行単位テキスト選択(Android版`TerminalScreen.kt`の
    /// `awaitLongPressOrCancellation`+ドラッグループと対称)。`UILongPressGestureRecognizer`は
    /// `.began`後の移動でも認識状態を維持し続けて`.changed`を報告し続けるため、
    /// 別途pan gestureを組み合わせる必要はない。
    @objc private func handleLongPress(_ recognizer: UILongPressGestureRecognizer) {
        guard let update = latestUpdate else { return }
        let cols = Int(update.cols)
        let rows = Int(update.rows)
        let point = recognizer.location(in: self)
        let cell = offsetToCellPos(x: point.x, y: point.y, cellWidth: cellSize.width, cellHeight: cellSize.height, cols: cols, rows: rows)

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
        guard let update = latestUpdate else { return }
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
