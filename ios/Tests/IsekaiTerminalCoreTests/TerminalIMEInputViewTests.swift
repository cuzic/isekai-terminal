import XCTest
@testable import IsekaiTerminalCore
import IsekaiTerminalCoreLogic

/// Phase 1A-5: 日本語IME単体スパイクの検証。
///
/// `UITextInput`プロトコルのメソッド(`setMarkedText`/`unmarkText`/`insertText`)を
/// 実際のIMEが呼び出すのと同じ形で直接呼び出し、変換ロジックが正しく動くことを
/// GitHub Actions上で検証する。候補ウィンドウの見た目そのものはCIでは検証できないため、
/// 実機/シミュレータでの目視確認は別途行う(PLAN.md「Phase Y」節参照)。
final class TerminalIMEInputViewTests: XCTestCase {
    func testRomajiConversionThenBackspaceThenConfirm() {
        let view = TerminalIMEInputView()

        // ローマ字入力が「こんにちは」まで変換された状態をシミュレート。
        view.setMarkedText("こんにちは", selectedRange: NSRange(location: 5, length: 0))
        // 変換中のBackspaceは、実際のIMEでは setMarkedText の呼び直しとして届く。
        view.setMarkedText("こんにち", selectedRange: NSRange(location: 4, length: 0))
        // 候補確定。
        view.unmarkText()

        XCTAssertEqual(view.committedText, "こんにち")
        XCTAssertEqual(view.markedTextLog, ["こんにちは", "こんにち"])
        XCTAssertNil(view.markedTextRange)
    }

    func testCancelledConversionDoesNotCommitAnything() {
        let view = TerminalIMEInputView()

        view.setMarkedText("た", selectedRange: NSRange(location: 1, length: 0))
        // ユーザーが変換をキャンセルした場合、IMEはmarkedTextをnilにして呼び直す。
        view.setMarkedText(nil, selectedRange: NSRange(location: 0, length: 0))

        XCTAssertEqual(view.committedText, "")
        XCTAssertNil(view.markedTextRange)
    }

    func testInsertTextAfterMarkedTextCommitsMarkedTextFirst() {
        let view = TerminalIMEInputView()

        view.setMarkedText("ねこ", selectedRange: NSRange(location: 2, length: 0))
        // 候補選択後、確定操作(unmarkText相当)を経ずに直接次の文字が来るケース
        // (一部のIMEはスペースキー確定などでinsertTextを直接呼ぶことがある)。
        view.insertText("は")

        XCTAssertEqual(view.committedText, "ねこは")
    }

    func testEmojiInsertedDirectlyWithoutMarkedText() {
        let view = TerminalIMEInputView()

        view.insertText("😀")

        XCTAssertEqual(view.committedText, "😀")
        XCTAssertTrue(view.markedTextLog.isEmpty)
    }

    func testMultiLinePaste() {
        let view = TerminalIMEInputView()

        view.insertText("line1\nline2")

        XCTAssertEqual(view.committedText, "line1\nline2")
    }

    func testDeleteBackwardOnCommittedTextOnly() {
        let view = TerminalIMEInputView()

        view.insertText("abc")
        view.deleteBackward()

        XCTAssertEqual(view.committedText, "ab")
    }

    // MARK: - Phase 1D(#18b): ターミナル統合用フック(onSendBytes/ctrlArmed)

    func testInsertTextSendsCommitBytes() {
        let view = TerminalIMEInputView()
        var sent: [Data] = []
        view.onSendBytes = { sent.append($0) }

        view.insertText("a")

        XCTAssertEqual(sent, [Data("a".utf8)])
    }

    func testCommittingMarkedTextSendsBytesOnce() {
        let view = TerminalIMEInputView()
        var sent: [Data] = []
        view.onSendBytes = { sent.append($0) }

        view.setMarkedText("こんにちは", selectedRange: NSRange(location: 5, length: 0))
        view.unmarkText()

        XCTAssertEqual(sent, [Data("こんにちは".utf8)])
    }

    func testDeleteBackwardAlwaysSendsDelByteEvenWhenBufferEmpty() {
        let view = TerminalIMEInputView()
        var sent: [Data] = []
        view.onSendBytes = { sent.append($0) }

        // バッファは空(何も入力していない)でも、ターミナル側には削除すべき文字が
        // あり得るため、常にDELバイトを送信する。
        view.deleteBackward()

        XCTAssertEqual(sent, [Data([0x7F])])
    }

    func testCtrlArmedConvertsNextSingleCharacterToControlByte() {
        let view = TerminalIMEInputView()
        var sent: [Data] = []
        view.onSendBytes = { sent.append($0) }

        view.ctrlArmed = true
        view.insertText("c")

        XCTAssertEqual(sent, [Data([0x03])]) // Ctrl+C
        XCTAssertFalse(view.ctrlArmed, "1文字処理したら自動的にOFFへ戻る")
    }

    func testCtrlArmedFallsBackToNormalCommitForMultiCharacterInsert() {
        let view = TerminalIMEInputView()
        var sent: [Data] = []
        view.onSendBytes = { sent.append($0) }

        view.ctrlArmed = true
        view.insertText("ab")

        XCTAssertEqual(sent, [terminalCommitTextBytes(text: "ab", bracketedPasteMode: false)])
        XCTAssertFalse(view.ctrlArmed)
    }

    func testCanBecomeFirstResponder() {
        let view = TerminalIMEInputView()
        XCTAssertTrue(view.canBecomeFirstResponder)
    }
}
