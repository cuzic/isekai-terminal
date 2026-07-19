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

    /// タスク#82: テンキー(numpad)のHID usage→`TerminalNumpadKey`。
    /// Android版`TerminalKeyEncoder`の`KC_NUMPAD_*`表と対になる。
    func testNumpadDigitsMapToNumpadKeys() {
        XCTAssertEqual(TerminalHardwareKeyMapper.numpadKey(for: .keypad0), .digit0)
        XCTAssertEqual(TerminalHardwareKeyMapper.numpadKey(for: .keypad1), .digit1)
        XCTAssertEqual(TerminalHardwareKeyMapper.numpadKey(for: .keypad2), .digit2)
        XCTAssertEqual(TerminalHardwareKeyMapper.numpadKey(for: .keypad3), .digit3)
        XCTAssertEqual(TerminalHardwareKeyMapper.numpadKey(for: .keypad4), .digit4)
        XCTAssertEqual(TerminalHardwareKeyMapper.numpadKey(for: .keypad5), .digit5)
        XCTAssertEqual(TerminalHardwareKeyMapper.numpadKey(for: .keypad6), .digit6)
        XCTAssertEqual(TerminalHardwareKeyMapper.numpadKey(for: .keypad7), .digit7)
        XCTAssertEqual(TerminalHardwareKeyMapper.numpadKey(for: .keypad8), .digit8)
        XCTAssertEqual(TerminalHardwareKeyMapper.numpadKey(for: .keypad9), .digit9)
    }

    func testNumpadOperatorsAndEnterMapToNumpadKeys() {
        XCTAssertEqual(TerminalHardwareKeyMapper.numpadKey(for: .keypadPeriod), .decimal)
        XCTAssertEqual(TerminalHardwareKeyMapper.numpadKey(for: .keypadComma), .comma)
        XCTAssertEqual(TerminalHardwareKeyMapper.numpadKey(for: .keypadPlus), .add)
        XCTAssertEqual(TerminalHardwareKeyMapper.numpadKey(for: .keypadHyphen), .subtract)
        XCTAssertEqual(TerminalHardwareKeyMapper.numpadKey(for: .keypadAsterisk), .multiply)
        XCTAssertEqual(TerminalHardwareKeyMapper.numpadKey(for: .keypadSlash), .divide)
        XCTAssertEqual(TerminalHardwareKeyMapper.numpadKey(for: .keypadEqualSign), .equals)
        XCTAssertEqual(TerminalHardwareKeyMapper.numpadKey(for: .keypadEnter), .enter)
    }

    /// NumLock(タスク#83で扱う別課題)とAS/400キーボード固有の`=`
    /// (`TerminalNumpadKey`に対応ケースが無い)は意図的に対象外(nil)。通常の
    /// 文字キー同様、既存の`UITextInput`経路にフォールスルーする。
    func testNumLockAndAs400EqualsAreNotMappedToNumpadKey() {
        XCTAssertNil(TerminalHardwareKeyMapper.numpadKey(for: .keypadNumLock))
        XCTAssertNil(TerminalHardwareKeyMapper.numpadKey(for: .keypadEqualSignAS400))
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
