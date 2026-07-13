import Foundation

/// 打鍵列編集/一覧UIでチップ・プレビューとして表示する短いラベル。
/// バイト変換([KeySequenceCommands.toBytes])とは独立した表示専用ロジック。
/// Android版`KeyStepLabels.kt`と対称。
extension KeyStep {
    public var shortLabel: String {
        switch self {
        case .ctrlChar(let c): return "^\(String(c).uppercased())"
        case .text(let t): return t
        case .special(let key): return key.shortLabel
        case .placeholderRef(let name): return "{\(name)}"
        }
    }
}

extension Array where Element == KeyStep {
    public var previewText: String { map(\.shortLabel).joined(separator: " ") }
}

extension TerminalKeyMapper.SpecialKey {
    var shortLabel: String {
        switch self {
        case .escape: return "Esc"
        case .tab: return "Tab"
        case .backspace: return "Backspace"
        case .delete: return "Delete"
        case .arrowUp: return "↑"
        case .arrowDown: return "↓"
        case .arrowLeft: return "←"
        case .arrowRight: return "→"
        case .home: return "Home"
        case .end: return "End"
        case .pageUp: return "PageUp"
        case .pageDown: return "PageDown"
        case .functionKey(let n): return "F\(n)"
        }
    }
}

/// 打鍵列編集画面のステップ追加UIで選べる特殊キーの一覧(ラベル付き)。
/// Android版`SPECIAL_KEY_CHOICES`と対称。Enterはこの一覧に含めない
/// ([TerminalKeyMapper.SpecialKey]にEnter相当のcaseが無いため — physical/software
/// Enterキーは通常のテキスト入力経路で`\r`として扱われる。打鍵列編集UI側では
/// `KeyStep.text("\r")`として追加する専用ボタンを別途用意する)。
public enum SpecialKeyChoices {
    public static let all: [(label: String, key: TerminalKeyMapper.SpecialKey)] = [
        ("Esc", .escape),
        ("Tab", .tab),
        ("Backspace", .backspace),
        ("Delete", .delete),
        ("↑", .arrowUp),
        ("↓", .arrowDown),
        ("←", .arrowLeft),
        ("→", .arrowRight),
        ("Home", .home),
        ("End", .end),
        ("PageUp", .pageUp),
        ("PageDown", .pageDown),
        ("F1", .functionKey(1)),
        ("F2", .functionKey(2)),
        ("F3", .functionKey(3)),
        ("F4", .functionKey(4)),
        ("F5", .functionKey(5)),
        ("F6", .functionKey(6)),
        ("F7", .functionKey(7)),
        ("F8", .functionKey(8)),
        ("F9", .functionKey(9)),
        ("F10", .functionKey(10)),
        ("F11", .functionKey(11)),
        ("F12", .functionKey(12)),
    ]
}
