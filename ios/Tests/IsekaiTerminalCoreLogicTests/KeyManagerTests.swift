import XCTest
@testable import IsekaiTerminalCoreLogic

/// Phase 1D: `KeyManager`(ed25519生成+OpenSSH PEMエンコード)の検証。
final class KeyManagerTests: XCTestCase {
    func testGeneratedPemHasExpectedOpenSshStructure() {
        let (pemBytes, authorizedKeysLine) = KeyManager.generateEd25519Pair()
        let pemText = String(decoding: pemBytes, as: UTF8.self)

        XCTAssertTrue(pemText.hasPrefix("-----BEGIN OPENSSH PRIVATE KEY-----\n"))
        XCTAssertTrue(pemText.hasSuffix("-----END OPENSSH PRIVATE KEY-----\n"))
        XCTAssertTrue(authorizedKeysLine.hasPrefix("ssh-ed25519 "))

        let body = pemText
            .replacingOccurrences(of: "-----BEGIN OPENSSH PRIVATE KEY-----\n", with: "")
            .replacingOccurrences(of: "-----END OPENSSH PRIVATE KEY-----\n", with: "")
            .replacingOccurrences(of: "\n", with: "")
        guard let decoded = Data(base64Encoded: body) else {
            XCTFail("PEM body is not valid base64")
            return
        }
        XCTAssertTrue(decoded.starts(with: Data("openssh-key-v1".utf8) + Data([0x00])))
    }

    func testTwoGenerationsProduceDifferentKeys() {
        let first = KeyManager.generateEd25519Pair()
        let second = KeyManager.generateEd25519Pair()
        XCTAssertNotEqual(first.authorizedKeysLine, second.authorizedKeysLine)
    }

    func testExtractPublicKeyHintReturnsPlaceholder() {
        let hint = KeyManager.extractPublicKeyHint(pemBytes: Data("dummy".utf8))
        XCTAssertFalse(hint.isEmpty)
    }

    /// 生成した鍵が実際にrussh(サーバー側sshd)で認証に使えることを、fixtureの
    /// authorized_keysへ追記して実接続することで検証する(golden byte比較ではなく
    /// 実際のプロトコル互換性を検証する、プロジェクトの既存方針に合わせる)。
    func testGeneratedKeyAuthenticatesAgainstRealSshd() async throws {
        guard let fixture = try? SshFixtureConfig.load() else {
            throw XCTSkip("SSH fixture not available at \(SshFixtureConfig.defaultPath); run start-sshd-fixture.sh first")
        }

        let (pemBytes, authorizedKeysLine) = KeyManager.generateEd25519Pair()

        let authorizedKeysURL = URL(fileURLWithPath: fixture.authorizedKeysPath)
        let existing = (try? String(contentsOf: authorizedKeysURL, encoding: .utf8)) ?? ""
        try (existing + "\n" + authorizedKeysLine + "\n").write(to: authorizedKeysURL, atomically: true, encoding: .utf8)

        let config = SshConfig(
            host: fixture.host,
            port: UInt16(fixture.port),
            username: fixture.user,
            auth: .publicKey(privateKeyPem: pemBytes),
            cols: 80,
            rows: 24,
            forwards: [],
            agentForward: false,
            jump: nil,
            allowNonLoopbackForwardBind: false
        )

        let recorder = KeyManagerAuthRecorder()
        let orchestrator = createSessionOrchestrator(callback: recorder)
        try orchestrator.connect(config: config)

        try await waitUntilFixtureCondition(timeout: 10) { await recorder.connected }
        orchestrator.disconnect()
        try await waitUntilFixtureCondition(timeout: 10) { await recorder.disconnected }
    }
}

private actor KeyManagerAuthRecorder: OrchestratorCallback {
    private(set) var connected = false
    private(set) var disconnected = false

    nonisolated func onData(data: Data) {}
    nonisolated func onHostKey(host: String, port: UInt16, fingerprint: String) -> Bool { true }
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
}
