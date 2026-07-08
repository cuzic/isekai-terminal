import UIKit
import IsekaiTerminalCoreLogic

/// Phase 1A-6: Rust→Swift画面更新ブリッジの最小レンダラー。
///
/// `DiagnosticFrameMailbox`(latest-wins)から取り出した`TerminalFrameBatch`を
/// Core Graphics/Core Textで描画する。初期版であり、性能問題を計測してから
/// Metalへの移行を検討する(ChatGPT外部レビュー2026-07-04、PLAN.md「Phase Y」節)。
/// 実際のVTE統合(rust-core `terminal`モジュール)はPhase 1Bで行う。
public final class TerminalFrameRenderer: UIView {
    private var currentFrame: TerminalFrameBatch?

    /// 実際に適用(描画対象として採用)されたframeの数。テストからの観測用。
    public private(set) var appliedFrameCount: Int = 0
    /// 世代が古いという理由で無視されたframeの数。テストからの観測用。
    public private(set) var discardedStaleGenerationCount: Int = 0

    private let font = UIFont.monospacedSystemFont(ofSize: 14, weight: .regular)

    public override init(frame: CGRect) {
        super.init(frame: frame)
        backgroundColor = .black
    }

    public required init?(coder: NSCoder) {
        super.init(coder: coder)
        backgroundColor = .black
    }

    /// frameを描画対象として適用する。`DiagnosticFrameMailbox`側でも世代チェックを
    /// 行っているが、Rendererを直接使う経路(将来の別配送方式)に備えて自衛的に
    /// 二重チェックする。
    public func apply(_ frame: TerminalFrameBatch) {
        if let current = currentFrame, frame.screenGeneration < current.screenGeneration {
            discardedStaleGenerationCount += 1
            return
        }
        currentFrame = frame
        appliedFrameCount += 1
        setNeedsDisplay()
    }

    public override func draw(_ rect: CGRect) {
        guard let frame = currentFrame else { return }
        let lineHeight = font.lineHeight

        for (rowIndex, row) in frame.rows.enumerated() {
            let attributed = attributedString(for: row)
            let y = CGFloat(rowIndex) * lineHeight
            attributed.draw(at: CGPoint(x: 0, y: y))
        }
    }

    private func attributedString(for row: PackedRow) -> NSAttributedString {
        let text = row.text
        let attributed = NSMutableAttributedString(string: text)
        let fullRange = NSRange(location: 0, length: (text as NSString).length)
        attributed.addAttribute(.font, value: font, range: fullRange)
        attributed.addAttribute(.foregroundColor, value: UIColor.white, range: fullRange)

        for run in row.attributeRuns {
            let runRange = NSRange(location: Int(run.start), length: Int(run.length))
            guard runRange.location >= 0, runRange.location + runRange.length <= fullRange.length else {
                continue
            }
            attributed.addAttribute(.foregroundColor, value: color(fromArgb: run.fgArgb), range: runRange)
            attributed.addAttribute(.backgroundColor, value: color(fromArgb: run.bgArgb), range: runRange)
            if run.bold {
                attributed.addAttribute(
                    .font,
                    value: UIFont.monospacedSystemFont(ofSize: font.pointSize, weight: .bold),
                    range: runRange
                )
            }
            if run.underline {
                attributed.addAttribute(.underlineStyle, value: NSUnderlineStyle.single.rawValue, range: runRange)
            }
        }
        return attributed
    }

    private func color(fromArgb argb: UInt32) -> UIColor {
        let a = CGFloat((argb >> 24) & 0xFF) / 255.0
        let r = CGFloat((argb >> 16) & 0xFF) / 255.0
        let g = CGFloat((argb >> 8) & 0xFF) / 255.0
        let b = CGFloat(argb & 0xFF) / 255.0
        return UIColor(red: r, green: g, blue: b, alpha: a)
    }
}
