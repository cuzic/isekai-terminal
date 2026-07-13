import Foundation

/// 打鍵列(KeySequence)を構成する最小単位。Android版`KeyStep`(sealed class)と対称。
///
/// バイト列への変換ロジックは持たない(この型自体はRustに非依存)。変換は
/// [KeySequenceCommands.toBytes]が既存の[TerminalKeyMapper]/Rust委譲関数へ委譲する形で行う
/// (打鍵列専用の変換ロジックを新たに作らない)。
public enum KeyStep: Equatable {
    /// Ctrl+<英字> 相当の制御バイト。[TerminalKeyMapper.controlByte(for:)]へ委譲する。
    case ctrlChar(Character)

    /// リテラルテキスト。`terminalCommitTextBytes`(Rust委譲)へ委譲する(改行は`\r`に正規化)。
    case text(String)

    /// 特殊キー。ペイロードは新規型を作らず、既存の[TerminalKeyMapper.SpecialKey]をそのまま使う。
    case special(TerminalKeyMapper.SpecialKey)

    /// 打鍵列セット(パック)のテンプレート内でのみ使用するプレースホルダー参照(例: `{prefix}`)。
    /// [KeySequenceCommands.toBytes]に渡す前に、呼び出し側がパックのインストール値で具体的な
    /// [KeyStep]へ解決しておくこと。未解決のまま渡された場合は何もバイトを出力しない
    /// (呼び出し側の実装ミスに対する防御であり、正常系では発生しない想定)。
    case placeholderRef(String)
}

/// [KeyStep.special]のペイロード([TerminalKeyMapper.SpecialKey])とJSON永続化用の文字列表現との
/// 相互変換。Swiftの`SpecialKey`はAndroid版`TerminalKeyEncoder`のようなInt keyCodeベースではない
/// ため、ケース名の文字列をそのまま永続化キーとして使う(iOS側の正典表現に合わせる)。
extension TerminalKeyMapper.SpecialKey {
    var persistenceKey: String {
        switch self {
        case .escape: return "escape"
        case .tab: return "tab"
        case .backspace: return "backspace"
        case .delete: return "delete"
        case .arrowUp: return "arrowUp"
        case .arrowDown: return "arrowDown"
        case .arrowLeft: return "arrowLeft"
        case .arrowRight: return "arrowRight"
        case .home: return "home"
        case .end: return "end"
        case .pageUp: return "pageUp"
        case .pageDown: return "pageDown"
        case .functionKey: return "functionKey"
        }
    }

    /// [persistenceKey]と(`functionKey`の場合のみ)function key番号から復元する。
    /// 未知のキー名、または未対応のfunction key番号(`TerminalKeyMapper.bytes(for:)`が空配列を
    /// 返す番号)は`nil`を返す(JSON復元時にそのstepだけをスキップするための簡易バリデーション)。
    static func from(persistenceKey: String, functionKeyNumber: Int?) -> TerminalKeyMapper.SpecialKey? {
        let key: TerminalKeyMapper.SpecialKey?
        switch persistenceKey {
        case "escape": key = .escape
        case "tab": key = .tab
        case "backspace": key = .backspace
        case "delete": key = .delete
        case "arrowUp": key = .arrowUp
        case "arrowDown": key = .arrowDown
        case "arrowLeft": key = .arrowLeft
        case "arrowRight": key = .arrowRight
        case "home": key = .home
        case "end": key = .end
        case "pageUp": key = .pageUp
        case "pageDown": key = .pageDown
        case "functionKey":
            guard let n = functionKeyNumber, !TerminalKeyMapper.bytes(for: .functionKey(n)).isEmpty else { return nil }
            key = .functionKey(n)
        default:
            key = nil
        }
        return key
    }
}
