import XCTest
@testable import IsekaiTerminalCoreLogic

/// Android版`KeySequencePackResolverTest.kt`と同じ観点の検証。
final class KeySequencePackResolverTests: XCTestCase {

    func testResolvesTmuxPackSequencesUsingInstallationParamValues() {
        let paramValues: [String: KeyStep] = ["prefix": .ctrlChar("b")]
        let resolved = KeySequencePackResolver.resolve(pack: KeySequencePacks.tmux, paramValues: paramValues)

        XCTAssertEqual(resolved.count, KeySequencePacks.tmux.sequences.count)
        let newWindow = resolved.first { $0.label == "新規ウィンドウ" }!
        XCTAssertEqual(newWindow.steps, [.ctrlChar("b"), .text("c")])
        XCTAssertEqual(newWindow.packId, "tmux")
    }

    func testChangingPrefixParamImmediatelyChangesAllResolvedSequences() {
        // ユーザーがprefixキーをCtrl+BからCtrl+A(screen互換)へ変更した場合、
        // installationのparamValuesを1箇所変えるだけでパック内の全ボタンへ反映されること
        // (有効化時に打鍵列を複製する「マテリアライズ方式」ではないことの確認)。
        let before = KeySequencePackResolver.resolve(pack: KeySequencePacks.tmux, paramValues: ["prefix": .ctrlChar("b")])
        let after = KeySequencePackResolver.resolve(pack: KeySequencePacks.tmux, paramValues: ["prefix": .ctrlChar("a")])

        for seq in before {
            XCTAssertTrue(seq.steps.contains(.ctrlChar("b")))
        }
        for seq in after {
            XCTAssertTrue(seq.steps.contains(.ctrlChar("a")))
            XCTAssertFalse(seq.steps.contains(.ctrlChar("b")))
        }
    }

    func testMissingParamValueFallsBackToThePacksDefault() {
        let resolved = KeySequencePackResolver.resolve(pack: KeySequencePacks.tmux, paramValues: [:])
        let newWindow = resolved.first { $0.label == "新規ウィンドウ" }!
        // TMUXパックのdefaultはCtrl+B。
        XCTAssertEqual(newWindow.steps, [.ctrlChar("b"), .text("c")])
    }

    func testUnknownPlaceholderNameWithNoDefaultIsLeftUnresolvedAndProducesNoBytes() {
        let pack = KeySequencePack(
            id: "test", version: 1, name: "test",
            params: [], // "prefix" という名前のparamは定義されていない
            sequences: [KeySequencePackTemplate(label: "x", steps: [.placeholderRef("prefix"), .text("c")])]
        )
        let resolved = KeySequencePackResolver.resolve(pack: pack, paramValues: [:])
        XCTAssertEqual(resolved.first!.steps, [.placeholderRef("prefix"), .text("c")])
        // 未解決のplaceholderRefはKeySequenceCommands.toBytesで何も出力しない(安全側の挙動)。
        XCTAssertEqual(KeySequenceCommands.toBytes(resolved.first!.steps), Data("c".utf8))
    }

    func testTmuxPackSequenceWithADoubleQuoteStepResolvesCorrectly() {
        let resolved = KeySequencePackResolver.resolve(pack: KeySequencePacks.tmux, paramValues: ["prefix": .ctrlChar("b")])
        let splitVertical = resolved.first { $0.label == "ペイン分割(上下)" }!
        XCTAssertEqual(splitVertical.steps, [.ctrlChar("b"), .text("\"")])
        XCTAssertEqual(KeySequenceCommands.toBytes(splitVertical.steps), Data([0x02, UInt8(ascii: "\"")]))
    }
}
