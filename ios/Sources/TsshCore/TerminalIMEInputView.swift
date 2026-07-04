import UIKit

/// Phase 1A-5: 日本語IME単体スパイク。
///
/// 完全なターミナル統合(カーソル位置への配置、スクロール追従等)は#18bで行う。
/// ここでは`UITextInput`プロトコルを実装した最小のUIKit viewを用意し、
/// marked text(変換中文字列)の保持・確定・変換中のBackspace・候補選択が
/// XCTestから直接検証できることを確認する。
///
/// `XCUIApplication().typeText(_:)`はソフトウェアキーボード/IMEを経由せず
/// テキストを直接挿入するだけなので、変換ロジックの検証には使えない。
/// 一方、ここで実装した`setMarkedText`/`unmarkText`/`insertText`は実際の
/// 日本語IMEがこの view に対して呼び出すのと**同じ**UITextInputのメソッドであり、
/// これらをXCTestから直接呼び出すことで、候補ウィンドウの見た目そのもの以外の
/// 変換ロジックはCI上で検証できる。
public final class TerminalIMEInputView: UIView, UIKeyInput, UITextInput {

    // MARK: - 内部バッファ(単純化のため属性やスタイルは持たない)

    private var buffer: String = ""
    private var markedRange: NSRange?
    private var _selectedTextRange = IndexedTextRange(range: NSRange(location: 0, length: 0))

    /// 確定済みテキストの現在値。テストからの観測用。
    public private(set) var committedText: String = ""
    /// `setMarkedText`に渡された値をそのまま記録する。テストからの観測用。
    public private(set) var markedTextLog: [String?] = []

    public override init(frame: CGRect) {
        super.init(frame: frame)
    }

    public required init?(coder: NSCoder) {
        super.init(coder: coder)
    }

    public override var canBecomeFirstResponder: Bool { true }

    // MARK: - UIKeyInput

    public var hasText: Bool { !buffer.isEmpty }

    public func insertText(_ text: String) {
        // marked textが残っている状態でinsertTextが来た場合は、まずそれを確定してから
        // 新しいテキストを追加する(実際のIME/UIKitの挙動に合わせる)。
        commitMarkedTextIfNeeded()
        buffer += text
        committedText = buffer
        _selectedTextRange = IndexedTextRange(range: NSRange(location: (buffer as NSString).length, length: 0))
    }

    public func deleteBackward() {
        // 変換中(marked textあり)のBackspaceは、実際のIMEでは`setMarkedText`の
        // 呼び直しとして表現される(このviewのdeleteBackwardは呼ばれない)。
        // ここでは確定済みバッファに対する通常のBackspaceのみを扱う。
        guard markedRange == nil, !buffer.isEmpty else { return }
        buffer.removeLast()
        committedText = buffer
    }

    // MARK: - UITextInput: marked text

    public var markedTextStyle: [NSAttributedString.Key: Any]?

    public var markedTextRange: UITextRange? {
        guard let markedRange else { return nil }
        return IndexedTextRange(range: markedRange)
    }

    public func setMarkedText(_ markedText: String?, selectedRange: NSRange) {
        markedTextLog.append(markedText)
        if let markedText, !markedText.isEmpty {
            markedRange = NSRange(location: (buffer as NSString).length, length: (markedText as NSString).length)
        } else {
            markedRange = nil
        }
    }

    public func unmarkText() {
        commitMarkedTextIfNeeded()
    }

    private func commitMarkedTextIfNeeded() {
        if markedRange != nil, let text = markedTextLog.last, let text {
            buffer += text
            committedText = buffer
        }
        markedRange = nil
    }

    // MARK: - UITextInput: selection

    public var selectedTextRange: UITextRange? {
        get { _selectedTextRange }
        set { _selectedTextRange = (newValue as? IndexedTextRange) ?? IndexedTextRange(range: NSRange(location: 0, length: 0)) }
    }

    // MARK: - UITextInput: document boundaries

    public var beginningOfDocument: UITextPosition { IndexedTextPosition(index: 0) }
    public var endOfDocument: UITextPosition { IndexedTextPosition(index: (buffer as NSString).length) }

    public func textRange(from fromPosition: UITextPosition, to toPosition: UITextPosition) -> UITextRange? {
        guard let from = (fromPosition as? IndexedTextPosition)?.index,
              let to = (toPosition as? IndexedTextPosition)?.index else { return nil }
        return IndexedTextRange(range: NSRange(location: min(from, to), length: abs(to - from)))
    }

    public func position(from position: UITextPosition, offset: Int) -> UITextPosition? {
        guard let index = (position as? IndexedTextPosition)?.index else { return nil }
        let newIndex = index + offset
        guard newIndex >= 0, newIndex <= (buffer as NSString).length else { return nil }
        return IndexedTextPosition(index: newIndex)
    }

    public func position(from position: UITextPosition, in direction: UITextLayoutDirection, offset: Int) -> UITextPosition? {
        self.position(from: position, offset: offset)
    }

    public func compare(_ position: UITextPosition, to other: UITextPosition) -> ComparisonResult {
        guard let a = (position as? IndexedTextPosition)?.index,
              let b = (other as? IndexedTextPosition)?.index else { return .orderedSame }
        if a < b { return .orderedAscending }
        if a > b { return .orderedDescending }
        return .orderedSame
    }

    public func offset(from: UITextPosition, to: UITextPosition) -> Int {
        guard let a = (from as? IndexedTextPosition)?.index,
              let b = (to as? IndexedTextPosition)?.index else { return 0 }
        return b - a
    }

    public var inputDelegate: UITextInputDelegate?

    public lazy var tokenizer: UITextInputTokenizer = UITextInputStringTokenizer(textInput: self)

    public func position(within range: UITextRange, farthestIn direction: UITextLayoutDirection) -> UITextPosition? {
        guard let r = (range as? IndexedTextRange)?.range else { return nil }
        switch direction {
        case .left, .up:
            return IndexedTextPosition(index: r.location)
        default:
            return IndexedTextPosition(index: r.location + r.length)
        }
    }

    public func characterRange(byExtending position: UITextPosition, in direction: UITextLayoutDirection) -> UITextRange? {
        nil
    }

    public func baseWritingDirection(for position: UITextPosition, in direction: UITextStorageDirection) -> NSWritingDirection {
        .leftToRight
    }

    public func setBaseWritingDirection(_ writingDirection: NSWritingDirection, for range: UITextRange) {}

    public func firstRect(for range: UITextRange) -> CGRect { .zero }

    public func caretRect(for position: UITextPosition) -> CGRect { .zero }

    public func selectionRects(for range: UITextRange) -> [UITextSelectionRect] { [] }

    public func closestPosition(to point: CGPoint) -> UITextPosition? { endOfDocument }

    public func closestPosition(to point: CGPoint, within range: UITextRange) -> UITextPosition? { range.end }

    public func characterRange(at point: CGPoint) -> UITextRange? { nil }

    public func text(in range: UITextRange) -> String? {
        guard let r = (range as? IndexedTextRange)?.range else { return nil }
        let ns = buffer as NSString
        guard r.location >= 0, r.location + r.length <= ns.length else { return nil }
        return ns.substring(with: r)
    }

    public func replace(_ range: UITextRange, withText text: String) {
        guard let r = (range as? IndexedTextRange)?.range else { return }
        let ns = buffer as NSString
        buffer = ns.replacingCharacters(in: r, with: text)
        committedText = buffer
    }
}

/// 単純な整数(UTF-16コードユニット)オフセットをラップするだけの最小`UITextPosition`実装。
final class IndexedTextPosition: UITextPosition {
    let index: Int
    init(index: Int) { self.index = index }
}

/// 単純な`NSRange`をラップするだけの最小`UITextRange`実装。
final class IndexedTextRange: UITextRange {
    let range: NSRange
    override var start: UITextPosition { IndexedTextPosition(index: range.location) }
    override var end: UITextPosition { IndexedTextPosition(index: range.location + range.length) }
    override var isEmpty: Bool { range.length == 0 }
    init(range: NSRange) { self.range = range }
}
