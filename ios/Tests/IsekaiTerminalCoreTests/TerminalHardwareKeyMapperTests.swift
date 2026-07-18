import XCTest
@testable import IsekaiTerminalCore
import IsekaiTerminalCoreLogic

/// タスク#63: ハードウェアキーボード対応(`UIKeyboardHIDUsage`/`UIKeyModifierFlags`→
/// `TerminalKeyMapper.SpecialKey`/`TerminalKeyModifiers`)の変換表そのものの検証。
///
/// `UIKey`/`UIPress`自体は公開イニシャライザが無く実機/シミュレータの実際の
/// キー押下でしか生成できないため(`TerminalIMEInputView.pressesBegan`の統合的な
/// 検証はCIでは行えない)、ここでは`TerminalHardwareKeyMapper`が受け取る
/// `UIKeyboardHIDUsage`/`UIKeyModifierFlags`側は素の値として直接構築できることを
/// 利用し、変換ロジックだけを切り出して検証する。
final class TerminalHardwareKeyMapperTests: XCTestCase {
    func testArrowKeysMapToSpecialKeys() {
        XCTAssertEqual(TerminalHardwareKeyMapper.specialKey(for: .keyboardUpArrow), .arrowUp)
        XCTAssertEqual(TerminalHardwareKeyMapper.specialKey(for: .keyboardDownArrow), .arrowDown)
        XCTAssertEqual(TerminalHardwareKeyMapper.specialKey(for: .keyboardLeftArrow), .arrowLeft)
        XCTAssertEqual(TerminalHardwareKeyMapper.specialKey(for: .keyboardRightArrow), .arrowRight)
    }

    func testNavigationKeysMapToSpecialKeys() {
        XCTAssertEqual(TerminalHardwareKeyMapper.specialKey(for: .keyboardHome), .home)
        XCTAssertEqual(TerminalHardwareKeyMapper.specialKey(for: .keyboardEnd), .end)
        XCTAssertEqual(TerminalHardwareKeyMapper.specialKey(for: .keyboardPageUp), .pageUp)
        XCTAssertEqual(TerminalHardwareKeyMapper.specialKey(for: .keyboardPageDown), .pageDown)
        XCTAssertEqual(TerminalHardwareKeyMapper.specialKey(for: .keyboardEscape), .escape)
        XCTAssertEqual(TerminalHardwareKeyMapper.specialKey(for: .keyboardTab), .tab)
    }

    /// 前方削除キー(`.keyboardDeleteForward`)は`SpecialKey.delete`
    /// (Rust側`ForwardDelete`、`ESC[3~`)に対応する。
    func testForwardDeleteMapsToDeleteSpecialKey() {
        XCTAssertEqual(TerminalHardwareKeyMapper.specialKey(for: .keyboardDeleteForward), .delete)
    }

    /// 通常のBackspaceは既存の`TerminalIMEInputView.deleteBackward()`
    /// (`UIKeyInput`)が処理するため、ここでは意図的に対象外(nil)。
    func testRegularBackspaceIsNotMappedToAvoidDoubleSending() {
        XCTAssertNil(TerminalHardwareKeyMapper.specialKey(for: .keyboardDeleteOrBackspace))
    }

    func testFunctionKeysMapToFunctionKeySpecialKey() {
        XCTAssertEqual(TerminalHardwareKeyMapper.specialKey(for: .keyboardF1), .functionKey(1))
        XCTAssertEqual(TerminalHardwareKeyMapper.specialKey(for: .keyboardF12), .functionKey(12))
    }

    /// 通常の文字キーは`UITextInput`(`insertText`)の既存経路に任せるためnil。
    func testRegularLetterKeyIsNotMapped() {
        XCTAssertNil(TerminalHardwareKeyMapper.specialKey(for: .keyboardA))
    }

    func testModifiersMapAllFourFlagsIndependently() {
        XCTAssertEqual(
            TerminalHardwareKeyMapper.modifiers(for: []),
            TerminalKeyModifiers(shift: false, alt: false, ctrl: false, meta: false)
        )
        XCTAssertEqual(
            TerminalHardwareKeyMapper.modifiers(for: .shift),
            TerminalKeyModifiers(shift: true, alt: false, ctrl: false, meta: false)
        )
        XCTAssertEqual(
            TerminalHardwareKeyMapper.modifiers(for: .alternate),
            TerminalKeyModifiers(shift: false, alt: true, ctrl: false, meta: false)
        )
        XCTAssertEqual(
            TerminalHardwareKeyMapper.modifiers(for: .control),
            TerminalKeyModifiers(shift: false, alt: false, ctrl: true, meta: false)
        )
        XCTAssertEqual(
            TerminalHardwareKeyMapper.modifiers(for: .command),
            TerminalKeyModifiers(shift: false, alt: false, ctrl: false, meta: true)
        )
        XCTAssertEqual(
            TerminalHardwareKeyMapper.modifiers(for: [.shift, .control]),
            TerminalKeyModifiers(shift: true, alt: false, ctrl: true, meta: false)
        )
    }
}
