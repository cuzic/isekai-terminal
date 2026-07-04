import Foundation

/// `EventWakeListener`をSwiftのクロージャで実装するための薄いブリッジ
/// (`CallbackIngress.swift`のものと同型だが、frame配送用に独立させている)。
private final class FrameWakeBridge: EventWakeListener {
    private let onWake: @Sendable () -> Void

    init(onWake: @escaping @Sendable () -> Void) {
        self.onWake = onWake
    }

    func eventsAvailable() {
        onWake()
    }
}

/// Phase 1A-6: `DiagnosticFrameMailbox`(latest-wins)からのwake通知を受けて
/// `TerminalFrameRenderer`へ反映するActor。
///
/// `ControlEventQueue`(lossless、`CallbackIngress`が担当)とは異なるポリシーで、
/// 取りこぼしは許容し常に最新のframeだけを描画する。wake通知が多数連続しても
/// 実際に`take()`する頻度を`minFrameInterval`で制限し、Swift側の描画が
/// Rust側のVTE処理をブロックしないようにする(初期実装では約30fps相当)。
public actor FrameIngress {
    private let mailbox: DiagnosticFrameMailbox
    private let renderer: TerminalFrameRenderer
    private let minFrameInterval: TimeInterval
    private var lastAppliedAt: Date = .distantPast
    private var pendingWake = false

    public init(
        mailbox: DiagnosticFrameMailbox,
        renderer: TerminalFrameRenderer,
        minFrameInterval: TimeInterval = 1.0 / 30.0
    ) {
        self.mailbox = mailbox
        self.renderer = renderer
        self.minFrameInterval = minFrameInterval
    }

    public func start() {
        let bridge = FrameWakeBridge { [weak self] in
            guard let self else { return }
            Task { await self.handleWake() }
        }
        mailbox.setWakeListener(listener: bridge)
    }

    private func handleWake() async {
        let elapsed = Date().timeIntervalSince(lastAppliedAt)
        if elapsed < minFrameInterval {
            // 更新頻度の上限を超えた分は「取りに行く」こと自体を遅らせる。
            // mailboxはlatest-winsなので、遅延中に届いた分は自然に最新へ
            // 上書きされ、取りこぼしても問題ない。
            if !pendingWake {
                pendingWake = true
                let delay = minFrameInterval - elapsed
                Task {
                    try? await Task.sleep(nanoseconds: UInt64(delay * 1_000_000_000))
                    await self.drainIfNeeded()
                }
            }
            return
        }
        drainNow()
    }

    private func drainIfNeeded() {
        pendingWake = false
        drainNow()
    }

    private func drainNow() {
        guard let frame = mailbox.takeLatest() else { return }
        lastAppliedAt = Date()
        // TerminalFrameRenderer は UIView(MainActor隔離)なので、Actor自身の
        // 実行コンテキストから直接触らずMainActorへ明示的に渡す。
        let renderer = self.renderer
        Task { @MainActor in
            renderer.apply(frame)
        }
    }
}
