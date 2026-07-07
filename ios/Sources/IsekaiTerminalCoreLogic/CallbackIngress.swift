import Foundation

/// `EventWakeListener`(Rust callback_interface)をSwiftのクロージャで実装するための
/// 薄いブリッジ。`events_available()`はRustのどのスレッドからでも呼ばれ得るため、
/// ここでは即座に処理せず`onWake`クロージャへ通知するだけに徹する。
private final class EventWakeBridge: EventWakeListener {
    private let onWake: @Sendable () -> Void

    init(onWake: @escaping @Sendable () -> Void) {
        self.onWake = onWake
    }

    func eventsAvailable() {
        onWake()
    }
}

/// Phase 1A-4: 「Rust側の連番付きEventQueueが順序のSSOTであり、Swift側は
/// wake通知を受けてから能動的にdrainする」という設計を実証するActor。
///
/// 誤りだった旧設計(「Swift Actorで順序保証する」)との違い: このActorは
/// callbackから受け取ったデータそのものを直接状態に書き込まない。callbackは
/// 「取りに行ってよい」という合図(`events_available()`)だけを送り、実際の
/// 取得は`drainEvents(afterSequence:maxCount:)`をActor内から呼ぶことで行う。
/// Rust側のMutexで直列化された`sequence`がSSOTなので、複数スレッドから
/// callbackが飛んできてもActorが処理する順序は必ず`sequence`昇順になる。
///
/// 実際の`OrchestratorCallback`統合(ControlEventQueue/RenderMailboxの分離含む)は
/// Phase 1Cで行う。ここでは`DiagnosticEventQueue`を使った最小の骨格のみを持つ。
public actor CallbackIngress {
    private let queue: DiagnosticEventQueue
    private var lastSequence: UInt64 = 0
    private(set) var receivedMessages: [String] = []

    public init(queue: DiagnosticEventQueue) {
        self.queue = queue
    }

    /// wake通知の受信を開始する。呼び出し後にRust側で`push()`されたイベントは
    /// 自動的に`drain()`されて`receivedMessages`に反映される。
    public func start() {
        let bridge = EventWakeBridge { [weak self] in
            guard let self else { return }
            Task { await self.drain() }
        }
        queue.setWakeListener(listener: bridge)
    }

    /// 未処理のイベントを`sequence`昇順で取得し、`lastSequence`を進める。
    /// 通常は`start()`のwake通知経由で呼ばれるが、テストや明示的なポーリングにも使える。
    public func drain() {
        let events = queue.drainEvents(afterSequence: lastSequence, maxCount: 100)
        for event in events {
            lastSequence = event.sequence
            receivedMessages.append(event.message)
        }
    }
}
