import XCTest
@testable import TsshCore

/// Phase 1A-6: Rust→Swift画面更新ブリッジの検証。
/// `DiagnosticFrameMailbox`(latest-wins、Rust側でstaleなframeを破棄する)と
/// `TerminalFrameRenderer`(Renderer側でも自衛的に世代チェックする)が
/// 期待通りに連携することを確認する。
final class TerminalFrameRendererTests: XCTestCase {
    @MainActor
    func testAppliesNewerGenerationAndDiscardsOlder() {
        let renderer = TerminalFrameRenderer()

        renderer.apply(makeFrame(generation: 1, sequence: 1))
        renderer.apply(makeFrame(generation: 2, sequence: 1))
        // resize後に古い世代のframeが遅れて届いても適用してはいけない。
        renderer.apply(makeFrame(generation: 1, sequence: 99))

        XCTAssertEqual(renderer.appliedFrameCount, 2)
        XCTAssertEqual(renderer.discardedStaleGenerationCount, 1)
    }
}

final class FrameIngressTests: XCTestCase {
    @MainActor
    func testLatestFramePropagatesToRendererViaWakeNotification() async throws {
        let mailbox = DiagnosticFrameMailbox()
        let renderer = TerminalFrameRenderer()
        let ingress = FrameIngress(mailbox: mailbox, renderer: renderer, minFrameInterval: 0.01)
        await ingress.start()

        // 短時間に複数publishしても、latest-winsなので最終的に反映されるのは
        // 最新の1件でよい(取りこぼし自体は許容する設計)。
        mailbox.publish(frame: makeFrame(generation: 1, sequence: 1))
        mailbox.publish(frame: makeFrame(generation: 1, sequence: 2))
        mailbox.publish(frame: makeFrame(generation: 1, sequence: 3))

        var applied = 0
        for _ in 0..<300 {
            applied = renderer.appliedFrameCount
            if applied >= 1 { break }
            try await Task.sleep(nanoseconds: 10_000_000) // 10ms
        }

        XCTAssertGreaterThanOrEqual(applied, 1)
    }
}

private func makeFrame(generation: UInt64, sequence: UInt64) -> TerminalFrameBatch {
    TerminalFrameBatch(
        sessionId: "test-session",
        screenGeneration: generation,
        frameSequence: sequence,
        rows: [
            PackedRow(
                text: "hello",
                cellWidths: Data([1, 1, 1, 1, 1]),
                attributeRuns: [
                    AttributeRun(start: 0, length: 5, fgArgb: 0xFFFFFFFF, bgArgb: 0xFF000000, bold: false, underline: false)
                ]
            )
        ],
        dirtyTop: 0,
        dirtyBottom: 1,
        cursor: CursorState(row: 0, col: 5, visible: true),
        title: nil,
        bell: false
    )
}
