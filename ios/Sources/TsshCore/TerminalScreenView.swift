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

    public override init(frame: CGRect) {
        super.init(frame: frame)
        backgroundColor = .black
        contentMode = .redraw
        isOpaque = true
    }

    public required init?(coder: NSCoder) {
        super.init(coder: coder)
    }

    /// 最新の画面状態を反映する。`MainActor`から呼ぶこと。
    public func apply(_ update: ScreenUpdate) {
        latestUpdate = update
        setNeedsDisplay()
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
