import Foundation

/// ADR_FUNCTIONAL_CORE_EFFECTS.md §6 Step 8a′/Step 13: Rust(`reconnect_fsm.rs`)が世代付きで判断して
/// `OrchestratorCallback.onConnectionEdge`で届けた接続エッジを、対応する処理へそのまま振り分けるだけの転送層。
/// 重複排除・エッジ判定・世代の比較は一切しない(`.claude/rules/rust-ssot.md`。Rustは各世代について
/// `Established`の後、次の`Established`より前に`Lost`を正確に1回出す)。
///
/// `TerminalSessionController`(Apple専用ターゲット)の`onConnectionEdge`はここを通す。Linux上の
/// `swift test`(`ios-logic-linux-check.yml`)でもRustが生成したcallback契約golden
/// (`rust-core/tests/golden/callback_contract/`)をこの転送層へreplayできるよう、Logic層に置いている
/// (`CallbackContractGoldenReplayTests`)。
public enum ConnectionEdgeRouter {
    public static func route(
        edge: ConnectionEdge,
        generation: UInt64,
        onEstablished: (_ host: String, _ generation: UInt64) -> Void,
        onLost: (_ generation: UInt64) -> Void
    ) {
        switch edge {
        case .established(let host):
            onEstablished(host, generation)
        case .lost:
            onLost(generation)
        }
    }
}
