import XCTest
@testable import IsekaiTerminalCoreLogic

/// Phase 1B: TerminalKeyMapper(キー→制御シーケンス変換)の検証。
final class TerminalKeyMapperTests: XCTestCase {
    func testControlByteForLowercaseAndUppercase() {
        XCTAssertEqual(TerminalKeyMapper.controlByte(for: "c"), 0x03) // Ctrl+C
        XCTAssertEqual(TerminalKeyMapper.controlByte(for: "C"), 0x03)
        XCTAssertEqual(TerminalKeyMapper.controlByte(for: "d"), 0x04) // Ctrl+D
        XCTAssertEqual(TerminalKeyMapper.controlByte(for: "a"), 0x01) // Ctrl+A
        XCTAssertEqual(TerminalKeyMapper.controlByte(for: "z"), 0x1A) // Ctrl+Z
    }

    func testControlByteReturnsNilForDigitsAndNonAscii() {
        XCTAssertNil(TerminalKeyMapper.controlByte(for: "1"))
        XCTAssertNil(TerminalKeyMapper.controlByte(for: "あ"))
    }

    /// Rust側(`terminal_ctrl_byte`)への統合により、Android版と同じ
    /// `@ [ \ ] ^ _ ? space`もCtrl+<記号>として変換されるようになった
    /// (統合前のiOS版はアルファベットのみ対応していた)。
    func testControlByteSupportsAndroidParitySymbols() {
        XCTAssertEqual(TerminalKeyMapper.controlByte(for: "@"), 0x00)
        XCTAssertEqual(TerminalKeyMapper.controlByte(for: "["), 0x1B)
        XCTAssertEqual(TerminalKeyMapper.controlByte(for: "?"), 0x7F)
        XCTAssertEqual(TerminalKeyMapper.controlByte(for: " "), 0x00)
    }

    func testArrowKeySequences() {
        XCTAssertEqual(TerminalKeyMapper.bytes(for: .arrowUp), Array("\u{1B}[A".utf8))
        XCTAssertEqual(TerminalKeyMapper.bytes(for: .arrowDown), Array("\u{1B}[B".utf8))
        XCTAssertEqual(TerminalKeyMapper.bytes(for: .arrowRight), Array("\u{1B}[C".utf8))
        XCTAssertEqual(TerminalKeyMapper.bytes(for: .arrowLeft), Array("\u{1B}[D".utf8))
    }

    func testEscapeTabBackspaceDelete() {
        XCTAssertEqual(TerminalKeyMapper.bytes(for: .escape), [0x1B])
        XCTAssertEqual(TerminalKeyMapper.bytes(for: .tab), [0x09])
        XCTAssertEqual(TerminalKeyMapper.bytes(for: .backspace), [0x7F])
        XCTAssertEqual(TerminalKeyMapper.bytes(for: .delete), Array("\u{1B}[3~".utf8))
    }

    func testFunctionKeysF1ThroughF4UseSS3Form() {
        XCTAssertEqual(TerminalKeyMapper.bytes(for: .functionKey(1)), Array("\u{1B}OP".utf8))
        XCTAssertEqual(TerminalKeyMapper.bytes(for: .functionKey(2)), Array("\u{1B}OQ".utf8))
        XCTAssertEqual(TerminalKeyMapper.bytes(for: .functionKey(3)), Array("\u{1B}OR".utf8))
        XCTAssertEqual(TerminalKeyMapper.bytes(for: .functionKey(4)), Array("\u{1B}OS".utf8))
    }

    func testFunctionKeysF5ThroughF12UseCsiTildeForm() {
        XCTAssertEqual(TerminalKeyMapper.bytes(for: .functionKey(5)), Array("\u{1B}[15~".utf8))
        XCTAssertEqual(TerminalKeyMapper.bytes(for: .functionKey(12)), Array("\u{1B}[24~".utf8))
    }

    func testUnsupportedFunctionKeyReturnsEmpty() {
        XCTAssertEqual(TerminalKeyMapper.bytes(for: .functionKey(99)), [])
    }

    func testHomeEndPageUpPageDown() {
        XCTAssertEqual(TerminalKeyMapper.bytes(for: .home), Array("\u{1B}[H".utf8))
        XCTAssertEqual(TerminalKeyMapper.bytes(for: .end), Array("\u{1B}[F".utf8))
        XCTAssertEqual(TerminalKeyMapper.bytes(for: .pageUp), Array("\u{1B}[5~".utf8))
        XCTAssertEqual(TerminalKeyMapper.bytes(for: .pageDown), Array("\u{1B}[6~".utf8))
    }

    // ── 打鍵列(KeySequence)機能向けに追加した applicationCursorMode 対応オーバーロード ──

    func testBytesForKeyWithoutApplicationCursorModeUsesCsiArrowForm() {
        XCTAssertEqual(
            TerminalKeyMapper.bytes(for: .arrowUp, applicationCursorMode: false),
            Array("\u{1B}[A".utf8)
        )
    }

    func testBytesForKeyWithApplicationCursorModeUsesSs3ArrowForm() {
        XCTAssertEqual(
            TerminalKeyMapper.bytes(for: .arrowUp, applicationCursorMode: true),
            Array("\u{1B}OA".utf8)
        )
    }

    func testModeLessOverloadStillMatchesExplicitFalseForBackwardCompatibility() {
        XCTAssertEqual(
            TerminalKeyMapper.bytes(for: .arrowDown),
            TerminalKeyMapper.bytes(for: .arrowDown, applicationCursorMode: false)
        )
    }

    // ── #31: 修飾キー(Shift/Alt/Ctrl/Meta)付き特殊キー入力のUI配線 ──
    // ハードウェアキーボード接続時の想定挙動。実際の変換テーブルはRust側
    // (`terminal_special_key_bytes`のgoldenテスト、#29)がSSOTなので、ここでは
    // `TerminalKeyMapper.bytes`が`modifiers`引数をRust側へ正しく受け渡すことのみ検証する。

    private static let noModifiers = TerminalKeyModifiers(shift: false, alt: false, ctrl: false, meta: false)

    func testModifiersDefaultParameterMatchesExplicitNoModifiers() {
        XCTAssertEqual(
            TerminalKeyMapper.bytes(for: .arrowUp, applicationCursorMode: false),
            TerminalKeyMapper.bytes(for: .arrowUp, applicationCursorMode: false, modifiers: Self.noModifiers)
        )
    }

    /// Ctrl+矢印はDECCKMの値に関わらず常にCSI形式のパラメータ付きシーケンスになる
    /// (修飾子が1つでもあればSS3ではなくCSI、rust-core側のdocコメント参照)。
    func testCtrlArrowUsesCsiParameterFormRegardlessOfApplicationCursorMode() {
        let ctrl = TerminalKeyModifiers(shift: false, alt: false, ctrl: true, meta: false)
        XCTAssertEqual(
            TerminalKeyMapper.bytes(for: .arrowUp, applicationCursorMode: false, modifiers: ctrl),
            Array("\u{1B}[1;5A".utf8)
        )
        XCTAssertEqual(
            TerminalKeyMapper.bytes(for: .arrowUp, applicationCursorMode: true, modifiers: ctrl),
            Array("\u{1B}[1;5A".utf8)
        )
    }

    func testShiftHomeUsesCsiParameterForm() {
        let shift = TerminalKeyModifiers(shift: true, alt: false, ctrl: false, meta: false)
        XCTAssertEqual(
            TerminalKeyMapper.bytes(for: .home, applicationCursorMode: false, modifiers: shift),
            Array("\u{1B}[1;2H".utf8)
        )
    }

    /// F1〜F4は無修飾ならSS3形式だが、修飾子が付くとSS3では表現できないため
    /// CSI形式に切り替わる(`ESC[1;5P`等)。
    func testCtrlF1SwitchesFromSs3ToCsiForm() {
        let ctrl = TerminalKeyModifiers(shift: false, alt: false, ctrl: true, meta: false)
        XCTAssertEqual(
            TerminalKeyMapper.bytes(for: .functionKey(1), applicationCursorMode: false, modifiers: ctrl),
            Array("\u{1B}[1;5P".utf8)
        )
    }

    /// Shift+Tab → CBT(`ESC[Z`、readline/tmuxの「戻りタブ補完」に必要)。
    func testShiftTabProducesCbt() {
        let shift = TerminalKeyModifiers(shift: true, alt: false, ctrl: false, meta: false)
        XCTAssertEqual(
            TerminalKeyMapper.bytes(for: .tab, applicationCursorMode: false, modifiers: shift),
            Array("\u{1B}[Z".utf8)
        )
    }
}
