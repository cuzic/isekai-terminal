import XCTest
@testable import IsekaiTerminalCore
import IsekaiTerminalCoreLogic

/// Phase 1A-8: plain SSH最小縦切り。ハードコードした接続先(実際は
/// `rust-core/scripts/ios-fixture/start-sshd-fixture.sh`が起動するCI fixture)へ
/// 実際にSSH接続し、認証・PTY・echoコマンド・日本語送受信・切断が一通り動くことを
/// 実機/CIのiOS Simulator上で確認する(PLAN.md「Phase Y」節参照)。
///
/// fixture(`/tmp/ios-fixture/fixture.json`)が無い環境(通常のios-rust-core-check.ymlの
/// テスト実行等)ではXCTSkipし、既存のテストスイートを壊さないようにしている。
/// このテストは`.github/workflows/ios-ssh-vertical-slice-check.yml`が
/// fixtureを起動した上で明示的に実行する。
final class SshVerticalSliceTests: XCTestCase {
    func testConnectSendJapaneseTextAndDisconnect() async throws {
        guard let fixture = try? SshFixtureConfig.load() else {
            throw XCTSkip("SSH fixture not available at \(SshFixtureConfig.defaultPath); run start-sshd-fixture.sh first")
        }

        let privateKeyData = try Data(contentsOf: URL(fileURLWithPath: fixture.privateKeyPath))

        let config = SshConfig(
            host: fixture.host,
            port: UInt16(fixture.port),
            username: fixture.user,
            auth: .publicKey(privateKeyPem: privateKeyData),
            cols: 80,
            rows: 24,
            forwards: [],
            agentForward: false,
            jump: nil,
            allowNonLoopbackForwardBind: false
        )

        let recorder = SshVerticalSliceRecorder()
        let orchestrator = createSessionOrchestrator(callback: recorder)
        try orchestrator.connect(config: config)

        try await waitUntilFixtureCondition(timeout: 10) { await recorder.connected }

        // 日本語を含む文字列を送受信できることを確認する。
        let marker = "isekai-terminal-ios-spike-こんにちは-\(UUID().uuidString.prefix(8))"
        orchestrator.send(data: Data("echo \(marker)\n".utf8))

        try await waitUntilFixtureCondition(timeout: 10) {
            let text = await recorder.receivedText
            return text.contains(marker)
        }

        orchestrator.disconnect()

        try await waitUntilFixtureCondition(timeout: 10) { await recorder.disconnected }
    }
}

// MARK: - OrchestratorCallback記録用actor

/// `OrchestratorCallback`のメソッドはRustのtokioワーカースレッドから呼ばれ得るため、
/// actorで状態を保護する。callback自体は`nonisolated`にして即座に返し、
/// 実際の状態更新はTaskでactorへ委譲する(CallbackIngressと同じ考え方)。
private actor SshVerticalSliceRecorder: OrchestratorCallback {
    private(set) var connected = false
    private(set) var disconnected = false
    private var receivedBytes = Data()

    var receivedText: String {
        String(decoding: receivedBytes, as: UTF8.self)
    }

    nonisolated func onData(data: Data) {
        Task { await self.appendData(data) }
    }
    private func appendData(_ data: Data) {
        receivedBytes.append(data)
    }

    nonisolated func onHostKey(host: String, port: UInt16, fingerprint: String) -> Bool {
        // このスパイクではホスト鍵の信頼ストア(#31、Phase 1B)は未実装のため、
        // fixtureが動的に生成する鍵をそのまま受理する。
        true
    }

    nonisolated func onConnectionStateChanged(state: ConnectionPublicState) {
        switch state {
        case .connected:
            Task { await self.markConnected() }
        case .disconnected:
            Task { await self.markDisconnected() }
        default:
            break
        }
    }
    private func markConnected() { connected = true }
    private func markDisconnected() { disconnected = true }

    nonisolated func onScreenUpdate(update: ScreenUpdate) {}
    nonisolated func onTrzszStateChanged(state: TrzszPublicState) {}
    nonisolated func onDownloadComplete(fileName: String?, data: Data) {}
    nonisolated func onNoViablePath() {}
    nonisolated func onForwardStateChanged(id: String, state: ForwardState) {}
    nonisolated func onAgentSignRequest(keyFingerprint: String) -> Bool { false }
    nonisolated func onClipboardWrite(payload: ClipboardPayload) {}
    nonisolated func onClipboardPullRequest() -> ClipboardPayload? { nil }
    nonisolated func onRequestWifiFd() -> PlatformFd? { nil }
    nonisolated func onRequestCellularFd() -> PlatformFd? { nil }
    nonisolated func onRebindStateChanged(state: RebindPublicState) {}
    nonisolated func onPromptJump(target: PromptJumpTarget?) {}
    nonisolated func onPromptOutputCopyReady(text: String?) {}
    nonisolated func onFilePreviewResult(requestId: String, outcome: FilePreviewOutcome) {}
}
