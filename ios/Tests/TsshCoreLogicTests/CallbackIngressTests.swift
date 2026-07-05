import XCTest
@testable import TsshCoreLogic

/// Phase 1A-4: Swift Actorに順序保証を持たせるのではなく、Rust側の連番付き
/// EventQueueが順序のSSOTであることを確認する。ChatGPT外部レビュー
/// (2026-07-04、PLAN.md「Phase Y」節)で指摘された「Swift Task実行順は
/// 決定的FIFOではない」問題を、CallbackIngressの設計(wake通知→能動的drain)で
/// 回避できていることの検証。
final class CallbackIngressTests: XCTestCase {
    func testReceivesAllPushedEventsInSequenceOrder() async throws {
        let queue = DiagnosticEventQueue()
        let ingress = CallbackIngress(queue: queue)
        await ingress.start()

        let expectedCount = 20
        for i in 0..<expectedCount {
            queue.push(message: "event-\(i)")
        }

        var messages: [String] = []
        for _ in 0..<200 {
            messages = await ingress.receivedMessages
            if messages.count == expectedCount {
                break
            }
            try await Task.sleep(nanoseconds: 10_000_000) // 10ms
        }

        XCTAssertEqual(messages, (0..<expectedCount).map { "event-\($0)" })
    }

    func testDrainIsIdempotentWhenCalledDirectly() async {
        let queue = DiagnosticEventQueue()
        let ingress = CallbackIngress(queue: queue)

        queue.push(message: "a")
        queue.push(message: "b")

        await ingress.drain()
        await ingress.drain() // 2回目は新規イベントが無いので増えない

        let messages = await ingress.receivedMessages
        XCTAssertEqual(messages, ["a", "b"])
    }
}
