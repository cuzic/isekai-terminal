import Foundation
import XCTest
@testable import IsekaiTerminalCoreLogic

/// ADR_FUNCTIONAL_CORE_EFFECTS.md §6 Step 13: Rustが生成したcallback契約golden
/// (`rust-core/tests/golden/callback_contract/<scenario>.json`、生成・一致検査は
/// `rust-core/src/orchestrator/tests/callback_contract_golden.rs`)を、iOS側の転送層
/// (`ConnectionEdgeRouter`。`TerminalSessionController.onConnectionEdge`が使う)へreplayし、
/// 届いたエッジ1回につき対応処理が正確に1回呼ばれる(重複排除・エッジ判定・世代の書き換えをしない)ことを確かめる。
/// 公開状態のタグは生成されたUniFFI型(`ConnectionPublicState`)へ写せることを確かめる。
///
/// goldenは`rust-core/`配下の1か所だけにあり(コピー無し)、ここでは`#filePath`基準で直接読む
/// (`ios-logic-linux-check.yml`はリポジトリ全体をcheckoutした上でソースツリー内から`swift test`する)。
final class CallbackContractGoldenReplayTests: XCTestCase {
    /// `callback_contract_golden.rs`の`SCENARIOS`と同じ一覧。
    private static let scenarios = [
        "a_user_disconnect",
        "b_transport_error",
        "c_network_lost_debounce",
        "d_manual_connect_while_connected",
        "e_foreground_resume_reconnect",
        "f_reconnect_loop_success",
        "reconnect_gives_up",
        "fast_reconnect_cycles",
    ]

    private struct GoldenFile: Decodable {
        let scenario: String
        let events: [GoldenEvent]
    }

    private struct GoldenEvent: Decodable {
        let method: String
        let state: String?
        let host: String?
        let edge: String?
        let generation: UInt64?
        let didReconnect: Bool?

        enum CodingKeys: String, CodingKey {
            case method, state, host, edge, generation
            case didReconnect = "did_reconnect"
        }
    }

    /// 転送層から実際に呼ばれた処理の記録。
    private enum Delivered: Equatable {
        case established(host: String, generation: UInt64)
        case lost(generation: UInt64)
    }

    private static var goldenDir: URL {
        // <repo>/ios/Tests/IsekaiTerminalCoreLogicTests/<this file>
        URL(fileURLWithPath: #filePath)
            .deletingLastPathComponent()
            .deletingLastPathComponent()
            .deletingLastPathComponent()
            .deletingLastPathComponent()
            .appendingPathComponent("rust-core/tests/golden/callback_contract", isDirectory: true)
    }

    private func load(_ scenario: String) throws -> GoldenFile {
        let url = Self.goldenDir.appendingPathComponent("\(scenario).json")
        let data = try Data(contentsOf: url)
        let golden = try JSONDecoder().decode(GoldenFile.self, from: data)
        XCTAssertEqual(golden.scenario, scenario)
        return golden
    }

    private func publicState(_ event: GoldenEvent, scenario: String) throws -> ConnectionPublicState {
        switch event.state {
        case "Connecting": return .connecting
        case "Connected": return try .connected(host: XCTUnwrap(event.host, "\(scenario): Connectedにhostが無い"))
        case "Disconnected": return .disconnected(reason: nil, issueHint: nil)
        case "Reconnecting": return .reconnecting(elapsedSecs: 0, timeoutSecs: 60, reason: nil)
        case "Error": return .error(message: "golden")
        default: throw GoldenError.unknown("\(scenario): 未知の状態タグ \(String(describing: event.state))")
        }
    }

    private enum GoldenError: Error {
        case unknown(String)
    }

    private func replayAndAssertContract(_ scenario: String) throws {
        let golden = try load(scenario)
        var expected: [Delivered] = []
        var delivered: [Delivered] = []
        var states: [ConnectionPublicState] = []
        for event in golden.events {
            switch event.method {
            case "on_connection_edge":
                let generation = try XCTUnwrap(event.generation, "\(scenario): generationが無い")
                let edge: ConnectionEdge
                switch event.edge {
                case "Established":
                    let host = try XCTUnwrap(event.host, "\(scenario): Establishedにhostが無い")
                    edge = .established(host: host)
                    expected.append(.established(host: host, generation: generation))
                case "Lost":
                    edge = .lost
                    expected.append(.lost(generation: generation))
                default:
                    throw GoldenError.unknown("\(scenario): 未知のedge \(String(describing: event.edge))")
                }
                ConnectionEdgeRouter.route(
                    edge: edge,
                    generation: generation,
                    onEstablished: { host, generation in delivered.append(.established(host: host, generation: generation)) },
                    onLost: { generation in delivered.append(.lost(generation: generation)) }
                )
            case "on_connection_state_changed":
                let state = try publicState(event, scenario: scenario)
                states.append(state)
            case "on_foreground_resume":
                _ = try XCTUnwrap(event.didReconnect, "\(scenario): did_reconnectが無い")
            default:
                throw GoldenError.unknown("\(scenario): 未知のmethod \(event.method)")
            }
        }

        // エッジ1回につき対応処理が正確に1回、届いた順・世代のまま呼ばれる。
        XCTAssertEqual(delivered, expected, "\(scenario): 転送層が呼んだ処理の列")
        XCTAssertFalse(states.isEmpty, "\(scenario): 状態公開が1件も無い")

        // 届いた列がStep 8a′の契約を満たす(Rust側`assert_edge_contract`と同じ形)。
        var open: UInt64?
        var lastEstablished: UInt64?
        for item in delivered {
            switch item {
            case .established(_, let generation):
                XCTAssertNil(open, "\(scenario): Lostより前に次のEstablished(\(generation))が来た")
                if let last = lastEstablished {
                    XCTAssertGreaterThan(generation, last, "\(scenario): Establishedの世代が単調増加でない")
                }
                open = generation
                lastEstablished = generation
            case .lost(let generation):
                XCTAssertEqual(open, generation, "\(scenario): 対応するEstablishedの無いLost(\(generation))")
                open = nil
            }
        }
    }

    func testAUserDisconnect() throws { try replayAndAssertContract("a_user_disconnect") }
    func testBTransportError() throws { try replayAndAssertContract("b_transport_error") }
    func testCNetworkLostDebounce() throws { try replayAndAssertContract("c_network_lost_debounce") }
    func testDManualConnectWhileConnected() throws { try replayAndAssertContract("d_manual_connect_while_connected") }
    func testEForegroundResumeReconnect() throws { try replayAndAssertContract("e_foreground_resume_reconnect") }
    func testFReconnectLoopSuccess() throws { try replayAndAssertContract("f_reconnect_loop_success") }
    func testReconnectGivesUp() throws { try replayAndAssertContract("reconnect_gives_up") }
    func testFastReconnectCycles() throws { try replayAndAssertContract("fast_reconnect_cycles") }

    /// goldenディレクトリの全シナリオを上のテストがreplayしている(Rust側でシナリオを足したらここにも足す)。
    func testEveryGoldenScenarioIsReplayed() throws {
        let names = try FileManager.default.contentsOfDirectory(atPath: Self.goldenDir.path)
            .filter { $0.hasSuffix(".json") && !$0.hasSuffix(".actual.json") }
            .map { String($0.dropLast(".json".count)) }
            .sorted()
        XCTAssertEqual(names, Self.scenarios.sorted())
    }
}
