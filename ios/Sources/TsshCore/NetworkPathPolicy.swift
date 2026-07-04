import Foundation

/// Phase 1B: NWPathMonitorを接続可否のSSOTにせず「再接続のヒント」として扱う
/// ためのポリシー層(ChatGPT外部レビュー2026-07-04、PLAN.md「Phase Y」節参照)。
/// `NWPathMonitor`はネットワーク経路変化の通知に過ぎず、特定のSSHサーバー/
/// QUIC endpointへの到達性を保証しないため、実際の接続可否判断はtransport側
/// (Rust側SSOT)に委ねる。ここでは「いつRustへ通知するか」だけを決める。
///
/// 実際のセッション状態(Phase 1CのSessionState、Active/Degraded/Reconnecting等)
/// と統合する前に判断ロジック自体を単体テストできるよう、`SessionState`型そのもの
/// ではなく縮小版の`ConnectionHealthHint`を受け取ることで疎結合にしている。
public enum ConnectionHealthHint {
    case healthy
    case degradedOrReconnecting
}

public enum NetworkPathNotificationDecision: Equatable {
    case ignore
    case notifyImmediately
    case notifyAfterDebounce(interval: TimeInterval)
}

public struct NetworkPathPolicy {
    public let defaultDebounceInterval: TimeInterval

    public init(defaultDebounceInterval: TimeInterval = 0.4) {
        self.defaultDebounceInterval = defaultDebounceInterval
    }

    /// - Parameters:
    ///   - isSatisfied: `NWPath.status == .satisfied`かどうか。
    ///   - health: 現在の接続状態(将来的にはPhase 1CのSessionStateから導出する)。
    public func decide(isSatisfied: Bool, health: ConnectionHealthHint) -> NetworkPathNotificationDecision {
        switch health {
        case .healthy:
            // 接続が正常なら、pathが変化してもすぐには通知せず短時間coalesceする。
            // unsatisfiedになった場合も、ここでは切断と断定せずdebounce後に通知するだけに留める
            // (実際に切断すべきかどうかはtransport側のエラーで判断させる)。
            return .notifyAfterDebounce(interval: defaultDebounceInterval)
        case .degradedOrReconnecting:
            if isSatisfied {
                // 回復した可能性があるので即時通知し、再接続試行を前倒しする。
                return .notifyImmediately
            }
            // unsatisfiedのままでも即座に切断とは判断せず、実transport errorを待つ。
            return .ignore
        }
    }
}

/// 実際のNWPathMonitorイベントをポリシーに従ってRustへの通知に変換する。
/// `network_epoch`(単調増加)を発行し、debounce待ち中に新しいイベントが来たら
/// 古いepochの通知はキャンセルする。
public final class NetworkPathObserver {
    private let policy: NetworkPathPolicy
    private(set) var epoch: UInt64 = 0
    private var debounceTask: Task<Void, Never>?
    private let onNotify: (UInt64, Bool) -> Void

    /// - Parameter onNotify: `(network_epoch, isSatisfied)`。Rust側へのpath hint通知に対応する。
    public init(policy: NetworkPathPolicy = NetworkPathPolicy(), onNotify: @escaping (UInt64, Bool) -> Void) {
        self.policy = policy
        self.onNotify = onNotify
    }

    /// NWPathMonitorの`pathUpdateHandler`から呼ぶ想定。
    @discardableResult
    public func handlePathUpdate(isSatisfied: Bool, health: ConnectionHealthHint) -> NetworkPathNotificationDecision {
        epoch += 1
        let currentEpoch = epoch
        let decision = policy.decide(isSatisfied: isSatisfied, health: health)

        debounceTask?.cancel()
        switch decision {
        case .ignore:
            break
        case .notifyImmediately:
            onNotify(currentEpoch, isSatisfied)
        case .notifyAfterDebounce(let interval):
            debounceTask = Task { [weak self] in
                try? await Task.sleep(nanoseconds: UInt64(interval * 1_000_000_000))
                guard !Task.isCancelled, let self, self.epoch == currentEpoch else { return }
                self.onNotify(currentEpoch, isSatisfied)
            }
        }
        return decision
    }
}
