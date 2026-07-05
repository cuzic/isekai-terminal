import Foundation

/// Phase 1D: ターミナル本画面のSSH接続状態。UI(`TerminalView`)はこれだけを見て
/// 表示を切り替える(Rust SSOT原則: 接続状態の判断はここに集約し、UI側で
/// 独自のミラー状態を作らない)。
@MainActor
public final class TerminalUIState: ObservableObject {
    public enum State: Equatable {
        case connecting
        case connected
        case disconnected(reason: String?)
        case failed(message: String)
    }

    @Published public internal(set) var state: State = .connecting
    @Published public internal(set) var latestScreenUpdate: ScreenUpdate?

    public init() {}
}

/// Phase 1D: `ConnectionProfile`からSSH接続を開始し、`SessionCallback`を実装して
/// Rust側からのイベントを`TerminalUIState`へ橋渡しする。
///
/// `SessionCallback`のメソッドはRustのtokioワーカースレッドから直接呼ばれるため、
/// このクラス自体は`@MainActor`にせず(`onHostKey`が同期的にBoolを返す必要があり、
/// MainActorへのTask hopでは間に合わないため)、UIへ反映する`@Published`な状態は
/// 別クラス`TerminalUIState`(`@MainActor`)に分離し、`Task { @MainActor in }`で
/// 明示的に受け渡す。
public final class TerminalSessionController: SessionCallback, @unchecked Sendable {
    public let uiState = TerminalUIState()

    private let profile: ConnectionProfile
    private let password: String?
    private let db: ProfileDatabase
    private let vault: CredentialVault
    private let trustStore: SshHostTrustStore
    private var session: SshSession?

    public init(
        profile: ConnectionProfile,
        password: String?,
        db: ProfileDatabase = AppServices.shared.db,
        vault: CredentialVault = AppServices.shared.vault,
        trustStore: SshHostTrustStore
    ) {
        self.profile = profile
        self.password = password
        self.db = db
        self.vault = vault
        self.trustStore = trustStore
    }

    /// 接続を開始する。鍵認証の場合はCredentialVaultから秘密鍵を復号して使う。
    public func connect(cols: UInt32 = 80, rows: UInt32 = 24) {
        let auth: SshAuth
        if let keyEntryId = profile.keyEntryId {
            guard let keyEntry = try? db.fetchKeyEntry(id: keyEntryId) else {
                fail(message: "鍵情報が見つかりません")
                return
            }
            let metadata = CredentialVault.Metadata(keyId: keyEntry.id, keyType: keyEntry.keyType, publicKey: keyEntry.publicKey)
            guard let pemBytes = try? vault.retrieve(metadata: metadata) else {
                fail(message: "秘密鍵の復号に失敗しました")
                return
            }
            auth = .publicKey(privateKeyPem: pemBytes)
        } else {
            auth = .password(password: password ?? "")
        }

        let config = SshConfig(
            host: profile.host,
            port: UInt16(profile.port),
            username: profile.username,
            auth: auth,
            cols: cols,
            rows: rows,
            forwards: [],
            agentForward: false,
            jump: nil,
            allowNonLoopbackForwardBind: false
        )

        let newSession = createSshSession(config: config)
        session = newSession
        do {
            try newSession.connect(callback: self)
        } catch {
            fail(message: "\(error)")
        }
    }

    public func send(_ data: Data) {
        session?.send(data: data)
    }

    public func resize(cols: UInt32, rows: UInt32) {
        session?.resize(cols: cols, rows: rows)
    }

    public func disconnect() {
        session?.disconnect()
    }

    private func fail(message: String) {
        Task { @MainActor in self.uiState.state = .failed(message: message) }
    }

    // MARK: - SessionCallback

    public func onConnected() {
        Task { @MainActor in self.uiState.state = .connected }
    }

    public func onDisconnected(reason: String?) {
        Task { @MainActor in self.uiState.state = .disconnected(reason: reason) }
    }

    public func onScreenUpdate(update: ScreenUpdate) {
        Task { @MainActor in self.uiState.latestScreenUpdate = update }
    }

    /// ホスト鍵確認。iOS版は暫定的にTOFU(Trust On First Use)方式を採る
    /// (Android版`TerminalSession.kt`の`onHostKey`と同じ方針): 初回接続は
    /// 自動的に信頼して記録し、fingerprintが変化した場合のみ拒否する。
    /// `SshHostTrustStore`自体は対話的な確認UIを前提にした設計コメントが
    /// 付いているが、このcallbackはRustスレッドから同期的にBoolを返す必要があり
    /// (接続処理をブロックしてまでUI確認を待つ設計は複雑さに見合わないため)、
    /// 最初の実装ではAndroidと同じ自動信頼方式を踏襲する。対話的な確認UIへの
    /// 格上げは将来の改善候補(PLAN.md参照)。
    public func onHostKey(fingerprint: String) -> Bool {
        let identifier = SshHostTrustStore.makeIdentifier(kind: .sshHost, host: profile.host, port: UInt16(profile.port))
        switch trustStore.verify(identifier: identifier, keyType: "ssh", fingerprint: fingerprint) {
        case .trustedMatch:
            return true
        case .unknownHost:
            try? trustStore.trust(identifier: identifier, keyType: "ssh", fingerprint: fingerprint)
            return true
        case .mismatch:
            fail(message: "ホスト鍵が変更されています(なりすましの可能性)。接続を中止しました。")
            return false
        }
    }

    public func onData(data: Data) {}
    public func onTrzszRequest(transferId: String, mode: String, suggestedName: String?, expectedSize: UInt64?) {}
    public func onTrzszDownloadChunk(transferId: String, data: Data, isLast: Bool) {}
    public func onTrzszProgress(transferId: String, transferred: UInt64, total: UInt64?) {}
    public func onTrzszFinished(transferId: String, success: Bool, message: String?) {}
    public func onNoViablePath() {}
    public func onForwardStateChanged(id: String, state: ForwardState) {}
    public func onAgentSignRequest(keyFingerprint: String) -> Bool { false }
}
