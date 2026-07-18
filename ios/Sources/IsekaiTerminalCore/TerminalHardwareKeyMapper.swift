import UIKit
import IsekaiTerminalCoreLogic

/// タスク#63: 外付け(Bluetooth/USB)ハードウェアキーボードの物理キー入力経路。
///
/// `TerminalIMEInputView`の`UITextInput`実装(`insertText`/`deleteBackward`)は
/// ソフトウェアキーボード/IMEおよびハードウェアキーボードの「通常文字入力・
/// Backspace」を引き続きそのまま処理するが、矢印・Home/End・PageUp/PageDown・
/// Escape・Tab・前方削除・F1〜F12はUIKitの標準テキスト入力経路に乗らず、
/// `UIResponder.pressesBegan(_:with:)`(`UIPress`/`UIKey`)経由でしか観測できない
/// (実装前のgrep確認: `pressesBegan`/`UIKeyCommand`/`keyCommands`がios/Sources
/// 全体に0件だった)。
///
/// `UIKeyboardHIDUsage`(押された物理キーの種別)→`TerminalKeyMapper.SpecialKey`の
/// 対応づけは、Androidの`KeyEvent.keyCode`と同じくプラットフォーム固有の値なので
/// この層(iOS側)に置く。これはrust-core`terminal_special_key_bytes`のdocコメントが
/// 明示する設計方針(「どの物理/仮想キーが押されたか」の判定は各OS側、変換後の
/// `TerminalSpecialKey`だけをRustへ渡す)通りであり、rust-ssot.mdが対象とする
/// セッション/接続状態の判断ロジックには当たらない(状態を持たない純粋な変換表)。
enum TerminalHardwareKeyMapper {
    /// マッチしない(=通常の文字入力/Backspace等、既存の`UITextInput`経路に
    /// フォールスルーすべき)場合は`nil`を返す。
    ///
    /// 通常のBackspace(`.keyboardDeleteOrBackspace`)はここに含めない: 既存の
    /// `TerminalIMEInputView.deleteBackward()`(`UIKeyInput`)がハードウェア
    /// キーボードのBackspaceも引き続き処理するため、ここでも扱うと二重送信に
    /// なる。前方削除キー(`.keyboardDeleteForward`)は`UITextInput`側に対応する
    /// フックが無く今まで未対応だったため、`SpecialKey.delete`(Rust側
    /// `ForwardDelete`、`ESC[3~`)としてここで新たに扱う。
    static func specialKey(for hidUsage: UIKeyboardHIDUsage) -> TerminalKeyMapper.SpecialKey? {
        switch hidUsage {
        case .keyboardEscape: return .escape
        case .keyboardTab: return .tab
        case .keyboardDeleteForward: return .delete
        case .keyboardUpArrow: return .arrowUp
        case .keyboardDownArrow: return .arrowDown
        case .keyboardLeftArrow: return .arrowLeft
        case .keyboardRightArrow: return .arrowRight
        case .keyboardHome: return .home
        case .keyboardEnd: return .end
        case .keyboardPageUp: return .pageUp
        case .keyboardPageDown: return .pageDown
        case .keyboardF1: return .functionKey(1)
        case .keyboardF2: return .functionKey(2)
        case .keyboardF3: return .functionKey(3)
        case .keyboardF4: return .functionKey(4)
        case .keyboardF5: return .functionKey(5)
        case .keyboardF6: return .functionKey(6)
        case .keyboardF7: return .functionKey(7)
        case .keyboardF8: return .functionKey(8)
        case .keyboardF9: return .functionKey(9)
        case .keyboardF10: return .functionKey(10)
        case .keyboardF11: return .functionKey(11)
        case .keyboardF12: return .functionKey(12)
        default: return nil
        }
    }

    /// `UIKeyModifierFlags`→rust-core側`TerminalKeyModifiers`。
    static func modifiers(for flags: UIKeyModifierFlags) -> TerminalKeyModifiers {
        TerminalKeyModifiers(
            shift: flags.contains(.shift),
            alt: flags.contains(.alternate),
            ctrl: flags.contains(.control),
            meta: flags.contains(.command)
        )
    }
}
