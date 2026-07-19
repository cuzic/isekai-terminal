import XCTest
@testable import IsekaiTerminalCore
import IsekaiTerminalCoreLogic

/// Phase 1D(#18b): `TerminalSessionController.onHostKey`のホスト鍵確認ロジックの検証。
/// 未知ホストは自動trustせず`uiState.newHostKeyPrompt`を立てて一旦拒否し、
/// `trustNewHostKey()`経由の明示的な信頼後にのみ受理する(Android版`TerminalSession.kt`の
/// 既定`autoTrustNewHostKeys=false`と同じ方針、Codexアーキテクチャレビュー指摘の反映)。
/// 実際のSSH接続は行わず、ホスト鍵確認ロジックだけを直接呼び出す。
@MainActor
final class TerminalSessionControllerTests: XCTestCase {
    private func makeController(host: String = "127.0.0.1") throws -> (TerminalSessionController, SshHostTrustStore) {
        let db = try ProfileDatabase.inMemory()
        let tempDir = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
        try FileManager.default.createDirectory(at: tempDir, withIntermediateDirectories: true)
        let vault = try CredentialVault(blobDirectory: tempDir, keychainService: "test.terminalsession.\(UUID().uuidString)")
        let trustStore = try SshHostTrustStore(storeURL: tempDir.appendingPathComponent("trust.json"))

        let profile = ConnectionProfile(displayName: "test", host: host, port: 22, username: "tester")
        let controller = TerminalSessionController(profile: profile, password: "pw", db: db, vault: vault, trustStore: trustStore)
        return (controller, trustStore)
    }

    func testFirstConnectionShowsPromptAndRejectsUntilTrusted() async throws {
        let (controller, trustStore) = try makeController()

        XCTAssertFalse(controller.onHostKey(host: "127.0.0.1", port: 22, fingerprint: "SHA256:aaaa"))

        try await waitUntilFixtureCondition(timeout: 2) {
            await controller.uiState.newHostKeyPrompt != nil
        }
        XCTAssertEqual(controller.uiState.newHostKeyPrompt, NewHostKeyPrompt(host: "127.0.0.1", port: 22, fingerprint: "SHA256:aaaa"))

        let identifier = SshHostTrustStore.makeIdentifier(kind: .sshHost, host: "127.0.0.1", port: 22)
        XCTAssertNil(trustStore.record(for: identifier))

        controller.trustNewHostKey()

        XCTAssertNil(controller.uiState.newHostKeyPrompt)
        XCTAssertEqual(trustStore.record(for: identifier)?.fingerprint, "SHA256:aaaa")
        XCTAssertTrue(controller.onHostKey(host: "127.0.0.1", port: 22, fingerprint: "SHA256:aaaa"))
    }

    func testDismissNewHostKeyPromptDisconnectsWithoutTrusting() async throws {
        let (controller, trustStore) = try makeController()

        XCTAssertFalse(controller.onHostKey(host: "127.0.0.1", port: 22, fingerprint: "SHA256:aaaa"))
        try await waitUntilFixtureCondition(timeout: 2) {
            await controller.uiState.newHostKeyPrompt != nil
        }

        controller.dismissNewHostKeyPrompt()

        XCTAssertNil(controller.uiState.newHostKeyPrompt)
        let identifier = SshHostTrustStore.makeIdentifier(kind: .sshHost, host: "127.0.0.1", port: 22)
        XCTAssertNil(trustStore.record(for: identifier))
    }

    func testSameFingerprintIsAcceptedAfterTrust() async throws {
        let (controller, _) = try makeController()

        XCTAssertFalse(controller.onHostKey(host: "127.0.0.1", port: 22, fingerprint: "SHA256:aaaa"))
        try await waitUntilFixtureCondition(timeout: 2) {
            await controller.uiState.newHostKeyPrompt != nil
        }
        controller.trustNewHostKey()

        XCTAssertTrue(controller.onHostKey(host: "127.0.0.1", port: 22, fingerprint: "SHA256:aaaa"))
        XCTAssertTrue(controller.onHostKey(host: "127.0.0.1", port: 22, fingerprint: "SHA256:aaaa"))
    }

    func testChangedFingerprintAfterTrustIsRejectedAndSurfacesFailure() async throws {
        let (controller, _) = try makeController()

        XCTAssertFalse(controller.onHostKey(host: "127.0.0.1", port: 22, fingerprint: "SHA256:aaaa"))
        try await waitUntilFixtureCondition(timeout: 2) {
            await controller.uiState.newHostKeyPrompt != nil
        }
        controller.trustNewHostKey()
        XCTAssertTrue(controller.onHostKey(host: "127.0.0.1", port: 22, fingerprint: "SHA256:aaaa"))

        XCTAssertFalse(controller.onHostKey(host: "127.0.0.1", port: 22, fingerprint: "SHA256:bbbb"))

        try await waitUntilFixtureCondition(timeout: 2) {
            await controller.uiState.state != .connecting
        }
        guard case .failed = controller.uiState.state else {
            XCTFail("expected .failed state, got \(controller.uiState.state)")
            return
        }
    }

    // 実sshd接続+CredentialVault(Keychain)を伴うE2Eテストは、素のSwiftPM
    // テストバンドルではKeychainがerrSecMissingEntitlement(-34018)で失敗するため
    // (`CredentialVaultTests.swift`のコメント参照)、アプリホスト型の
    // `IsekaiTerminalAppTests`側に`TerminalSessionControllerE2ETests.swift`として
    // 置いている。

    // MARK: - Phase 1E-2: 踏み台(ProxyJump)の認証解決(実接続なし)

    func testConnectFailsWhenJumpKeyEntryNotFound() async throws {
        let db = try ProfileDatabase.inMemory()
        let tempDir = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
        try FileManager.default.createDirectory(at: tempDir, withIntermediateDirectories: true)
        let vault = try CredentialVault(blobDirectory: tempDir, keychainService: "test.terminalsession.jump.\(UUID().uuidString)")
        let trustStore = try SshHostTrustStore(storeURL: tempDir.appendingPathComponent("trust.json"))

        let profile = ConnectionProfile(
            displayName: "via jump",
            host: "internal.example.com",
            port: 22,
            username: "user",
            jumpHost: "bastion.example.com",
            jumpPort: 22,
            jumpUsername: "jumpuser",
            jumpKeyEntryId: "does-not-exist"
        )
        // 接続先自体はパスワード認証で解決させ(空パスワードバリデーション、タスク#7の対象外)、
        // 踏み台側の鍵未検出だけを検証する。
        let controller = TerminalSessionController(profile: profile, password: "mainpw", db: db, vault: vault, trustStore: trustStore)

        controller.connect()

        try await waitUntilFixtureCondition(timeout: 2) {
            await controller.uiState.state != .connecting
        }
        guard case .failed(let message) = controller.uiState.state else {
            XCTFail("expected .failed state, got \(controller.uiState.state)")
            return
        }
        XCTAssertTrue(message.contains("踏み台"))
    }

    func testConnectFailsWhenMainKeyEntryNotFound() async throws {
        let db = try ProfileDatabase.inMemory()
        let tempDir = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
        try FileManager.default.createDirectory(at: tempDir, withIntermediateDirectories: true)
        let vault = try CredentialVault(blobDirectory: tempDir, keychainService: "test.terminalsession.mainkey.\(UUID().uuidString)")
        let trustStore = try SshHostTrustStore(storeURL: tempDir.appendingPathComponent("trust.json"))

        let profile = ConnectionProfile(
            displayName: "test",
            host: "example.com",
            port: 22,
            username: "user",
            keyEntryId: "does-not-exist"
        )
        let controller = TerminalSessionController(profile: profile, password: nil, db: db, vault: vault, trustStore: trustStore)

        controller.connect()

        try await waitUntilFixtureCondition(timeout: 2) {
            await controller.uiState.state != .connecting
        }
        guard case .failed = controller.uiState.state else {
            XCTFail("expected .failed state, got \(controller.uiState.state)")
            return
        }
    }

    // MARK: - 認証バリデーション(Android版`AuthValidator`と対称、Codexレビュー指摘の反映)

    func testConnectFailsWhenPasswordIsNilAndNoKeyEntrySet() async throws {
        let db = try ProfileDatabase.inMemory()
        let tempDir = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
        try FileManager.default.createDirectory(at: tempDir, withIntermediateDirectories: true)
        let vault = try CredentialVault(blobDirectory: tempDir, keychainService: "test.terminalsession.emptypw.\(UUID().uuidString)")
        let trustStore = try SshHostTrustStore(storeURL: tempDir.appendingPathComponent("trust.json"))

        let profile = ConnectionProfile(displayName: "test", host: "example.com", port: 22, username: "user")
        let controller = TerminalSessionController(profile: profile, password: nil, db: db, vault: vault, trustStore: trustStore)

        controller.connect()

        try await waitUntilFixtureCondition(timeout: 2) {
            await controller.uiState.state != .connecting
        }
        guard case .failed(let message) = controller.uiState.state else {
            XCTFail("expected .failed state, got \(controller.uiState.state)")
            return
        }
        XCTAssertTrue(message.contains("パスワード"))
    }

    func testConnectFailsWhenPasswordIsEmptyStringAndNoKeyEntrySet() async throws {
        let db = try ProfileDatabase.inMemory()
        let tempDir = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
        try FileManager.default.createDirectory(at: tempDir, withIntermediateDirectories: true)
        let vault = try CredentialVault(blobDirectory: tempDir, keychainService: "test.terminalsession.emptypw2.\(UUID().uuidString)")
        let trustStore = try SshHostTrustStore(storeURL: tempDir.appendingPathComponent("trust.json"))

        let profile = ConnectionProfile(displayName: "test", host: "example.com", port: 22, username: "user")
        let controller = TerminalSessionController(profile: profile, password: "", db: db, vault: vault, trustStore: trustStore)

        controller.connect()

        try await waitUntilFixtureCondition(timeout: 2) {
            await controller.uiState.state != .connecting
        }
        guard case .failed(let message) = controller.uiState.state else {
            XCTFail("expected .failed state, got \(controller.uiState.state)")
            return
        }
        XCTAssertTrue(message.contains("パスワード"))
    }

    // MARK: - Phase 1E-4: SSH agent署名要求の確認フロー

    func testAgentSignRequestApprovedReturnsTrue() async throws {
        let (controller, _) = try makeController()

        let resultTask = Task.detached {
            controller.onAgentSignRequest(keyFingerprint: "SHA256:cccc")
        }

        try await waitUntilFixtureCondition(timeout: 2) {
            await controller.uiState.pendingAgentSignRequest != nil
        }
        XCTAssertEqual(controller.uiState.pendingAgentSignRequest?.fingerprint, "SHA256:cccc")
        controller.respondToAgentSignRequest(approved: true)

        let result = await resultTask.value
        XCTAssertTrue(result)
        try await waitUntilFixtureCondition(timeout: 2) {
            await controller.uiState.pendingAgentSignRequest == nil
        }
    }

    func testAgentSignRequestDeniedReturnsFalse() async throws {
        let (controller, _) = try makeController()

        let resultTask = Task.detached {
            controller.onAgentSignRequest(keyFingerprint: "SHA256:dddd")
        }

        try await waitUntilFixtureCondition(timeout: 2) {
            await controller.uiState.pendingAgentSignRequest != nil
        }
        controller.respondToAgentSignRequest(approved: false)

        let result = await resultTask.value
        XCTAssertFalse(result)
    }

    // MARK: - Phase 1A-9(#30): isekai-helper/QUIC最小縦切り(transportPreference分岐)
    //
    // 実際のネットワーク接続は行わず、Android版`ConnectionProfile.toSshConfig`/
    // `toIsekaiPipeQuicConfig`相当の純粋なconfig構築ロジックと、`transportPreference`に
    // 応じた分岐(未対応方式は`.failed`になること)だけを検証する。

    private func makeControllerWithProfile(
        _ profile: ConnectionProfile,
        password: String? = "pw",
        relayVault: RelayCredentialVault = RelayCredentialVault(keychainService: "test.relayvault.\(UUID().uuidString)")
    ) throws -> TerminalSessionController {
        let db = try ProfileDatabase.inMemory()
        let tempDir = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
        try FileManager.default.createDirectory(at: tempDir, withIntermediateDirectories: true)
        let vault = try CredentialVault(blobDirectory: tempDir, keychainService: "test.terminalsession.transport.\(UUID().uuidString)")
        let trustStore = try SshHostTrustStore(storeURL: tempDir.appendingPathComponent("trust.json"))
        return TerminalSessionController(profile: profile, password: password, db: db, vault: vault, relayVault: relayVault, trustStore: trustStore)
    }

    func testMakeSshConfigMapsProfileFieldsAndGatesAgentForwardOnKeyAuth() throws {
        let profile = ConnectionProfile(
            displayName: "test", host: "example.com", port: 2222, username: "user",
            enableAgentForward: true,
            forwards: [StoredPortForward(kind: .local, bindPort: 8080, remoteHost: "127.0.0.1", remotePort: 80)],
            allowNonLoopbackForwardBind: true
        )
        let controller = try makeControllerWithProfile(profile)

        // keyEntryIdが無い(パスワード認証)ため、enableAgentForward=trueでもgateされfalseになる。
        let config = controller.makeSshConfig(auth: .password(password: "pw"), jump: nil, cols: 80, rows: 24)

        XCTAssertEqual(config.host, "example.com")
        XCTAssertEqual(config.port, 2222)
        XCTAssertEqual(config.username, "user")
        XCTAssertFalse(config.agentForward)
        XCTAssertEqual(config.forwards.count, 1)
        XCTAssertTrue(config.allowNonLoopbackForwardBind)
    }

    func testMakeIsekaiPipeQuicConfigMapsProfileFields() throws {
        let profile = ConnectionProfile(displayName: "test", host: "example.com", port: 2222, username: "user")
        let controller = try makeControllerWithProfile(profile)
        let jump = JumpConfig(host: "bastion.example.com", port: 22, username: "jumpuser", auth: .password(password: "jp"))

        let config = controller.makeIsekaiPipeQuicConfig(auth: .password(password: "pw"), jump: jump, cols: 100, rows: 40)

        XCTAssertEqual(config.sshHost, "example.com")
        XCTAssertEqual(config.sshPort, 2222)
        XCTAssertEqual(config.username, "user")
        XCTAssertEqual(config.cols, 100)
        XCTAssertEqual(config.rows, 40)
        XCTAssertEqual(config.jump, jump)
        XCTAssertNil(config.bindPort)
    }

    // helperBindPortが以前は保存経路が無く常にnilになっていたバグの回帰テスト
    // (Codexアーキテクチャレビュー指摘、ProfileEditView側の修正とセット)。
    func testMakeIsekaiPipeQuicConfigMapsHelperBindPort() throws {
        let profile = ConnectionProfile(
            displayName: "test", host: "example.com", port: 2222, username: "user",
            helperBindPort: 45823
        )
        let controller = try makeControllerWithProfile(profile)

        let config = controller.makeIsekaiPipeQuicConfig(auth: .password(password: "pw"), jump: nil, cols: 80, rows: 24)

        XCTAssertEqual(config.bindPort, 45823)
    }

    // MARK: - Phase 1E-5(#44): STUN+SSHランデブーP2P(config構築のみ、実接続なし)

    func testMakeIsekaiStunP2pConfigUsesProfileStunServerWhenSet() throws {
        let profile = ConnectionProfile(
            displayName: "test", host: "example.com", port: 22, username: "user",
            stunServer: "stun.example.com:3478"
        )
        let controller = try makeControllerWithProfile(profile)

        let config = controller.makeIsekaiStunP2pConfig(auth: .password(password: "pw"), jump: nil, cols: 80, rows: 24)

        XCTAssertEqual(config.stunServers, ["stun.example.com:3478"])
    }

    func testMakeIsekaiStunP2pConfigFallsBackToDefaultWhenStunServerIsNilOrBlank() throws {
        for stunServer in [nil, "", "   "] {
            let profile = ConnectionProfile(
                displayName: "test", host: "example.com", port: 22, username: "user",
                stunServer: stunServer
            )
            let controller = try makeControllerWithProfile(profile)

            let config = controller.makeIsekaiStunP2pConfig(auth: .password(password: "pw"), jump: nil, cols: 80, rows: 24)

            XCTAssertEqual(config.stunServers, [defaultStunServer])
        }
    }

    func testMakeIsekaiStunP2pConfigSplitsCommaSeparatedStunServerIntoMultipleEntries() throws {
        let profile = ConnectionProfile(
            displayName: "test", host: "example.com", port: 22, username: "user",
            stunServer: "stun.example.com:3478, stun2.example.com:3478"
        )
        let controller = try makeControllerWithProfile(profile)

        let config = controller.makeIsekaiStunP2pConfig(auth: .password(password: "pw"), jump: nil, cols: 80, rows: 24)

        XCTAssertEqual(config.stunServers, ["stun.example.com:3478", "stun2.example.com:3478"])
    }

    // MARK: - Phase 1E-6(#45): MASQUE relay P2P(config構築のみ、実接続なし)
    //
    // 実際の暗号化/復号(`RelayCredentialVault`、Keychainに触れる)を伴うテストは
    // 素の`IsekaiTerminalCoreTests`では`errSecMissingEntitlement`になるため、アプリホスト型の
    // `IsekaiTerminalAppTests`側に`RelayCredentialVaultTests.swift`として置いている
    // (`CredentialVaultTests.swift`と同じ理由)。ここではKeychainに触れない
    // 「relayJwt未設定」経路だけを検証する。

    func testMakeIsekaiLinkRelayConfigReturnsNilWhenRelayJwtMissing() throws {
        let profile = ConnectionProfile(displayName: "test", host: "example.com", port: 22, username: "user")
        let controller = try makeControllerWithProfile(profile)

        let config = controller.makeIsekaiLinkRelayConfig(auth: .password(password: "pw"), jump: nil, cols: 80, rows: 24)

        XCTAssertNil(config)
    }

    func testConnectIsekaiLinkRelayFailsCleanlyWhenRelayJwtMissing() async throws {
        let profile = ConnectionProfile(
            displayName: "test", host: "example.com", port: 22, username: "user",
            transportPreference: .isekaiLinkRelayQuic
        )
        let controller = try makeControllerWithProfile(profile)

        controller.connect()

        try await waitUntilFixtureCondition(timeout: 2) {
            await controller.uiState.state != .connecting
        }
        guard case .failed(let message) = controller.uiState.state else {
            XCTFail("expected .failed state, got \(controller.uiState.state)")
            return
        }
        XCTAssertTrue(message.contains("復号"))
    }

    // MARK: - Phase 1E-7(#46): Tailscale⇔直接アドレスのマルチパス(config構築のみ、実接続なし)

    func testMakeMultipathIsekaiPipeQuicConfigMapsDirectAndCellularAddresses() throws {
        let profile = ConnectionProfile(
            displayName: "test", host: "tailscale.example.com", port: 22, username: "user",
            directAddress: "203.0.113.5:4433",
            cellularRemoteAddress: "[2001:db8::1]:4433"
        )
        let controller = try makeControllerWithProfile(profile)

        let config = controller.makeMultipathIsekaiPipeQuicConfig(auth: .password(password: "pw"), jump: nil, cols: 80, rows: 24)

        XCTAssertEqual(config.sshHost, "tailscale.example.com")
        XCTAssertEqual(config.directHost, "203.0.113.5:4433")
        XCTAssertEqual(config.cellularRemoteHost, "[2001:db8::1]:4433")
        XCTAssertNil(config.wifiFd)
        XCTAssertNil(config.wifiLocalIp)
        XCTAssertNil(config.cellularFd)
        XCTAssertNil(config.cellularLocalIp)
        XCTAssertNil(config.bindPort)
    }

    // helperBindPortが以前は保存経路が無く常にnilになっていたバグの回帰テスト
    // (Codexアーキテクチャレビュー指摘、ProfileEditView側の修正とセット)。
    func testMakeMultipathIsekaiPipeQuicConfigMapsHelperBindPort() throws {
        let profile = ConnectionProfile(
            displayName: "test", host: "tailscale.example.com", port: 22, username: "user",
            helperBindPort: 45823
        )
        let controller = try makeControllerWithProfile(profile)

        let config = controller.makeMultipathIsekaiPipeQuicConfig(auth: .password(password: "pw"), jump: nil, cols: 80, rows: 24)

        XCTAssertEqual(config.bindPort, 45823)
    }

    func testMakeMultipathIsekaiPipeQuicConfigTreatsBlankDirectAddressAsNil() throws {
        for directAddress in [nil, "", "   "] {
            let profile = ConnectionProfile(
                displayName: "test", host: "example.com", port: 22, username: "user",
                directAddress: directAddress
            )
            let controller = try makeControllerWithProfile(profile)

            let config = controller.makeMultipathIsekaiPipeQuicConfig(auth: .password(password: "pw"), jump: nil, cols: 80, rows: 24)

            XCTAssertNil(config.directHost)
        }
    }

    // MARK: - Phase 1F-3(#50): 配色テーマの解決(Global default → Profile default)

    func testResolveThemeUsesProfileThemeNameWhenSet() throws {
        let profile = ConnectionProfile(
            displayName: "test", host: "example.com", port: 22, username: "user",
            themeName: "Dracula"
        )
        let controller = try makeControllerWithProfile(profile)
        let defaults = UserDefaults(suiteName: "test.themes.\(UUID().uuidString)")!

        XCTAssertEqual(controller.resolveTheme(defaults: defaults), TerminalThemes.dracula)
    }

    func testResolveThemeFallsBackToGlobalDefaultWhenProfileThemeIsNil() throws {
        let profile = ConnectionProfile(displayName: "test", host: "example.com", port: 22, username: "user")
        let controller = try makeControllerWithProfile(profile)
        let defaults = UserDefaults(suiteName: "test.themes.\(UUID().uuidString)")!
        defaults.set("Nord", forKey: TerminalThemes.prefKey)

        XCTAssertEqual(controller.resolveTheme(defaults: defaults), TerminalThemes.nord)
    }

    func testResolveThemeFallsBackToDefaultDarkWhenNeitherIsSet() throws {
        let profile = ConnectionProfile(displayName: "test", host: "example.com", port: 22, username: "user")
        let controller = try makeControllerWithProfile(profile)
        let defaults = UserDefaults(suiteName: "test.themes.\(UUID().uuidString)")!

        XCTAssertEqual(controller.resolveTheme(defaults: defaults), TerminalThemes.defaultDark)
    }

    func testConnectWithUnsupportedTransportPreferenceFails() async throws {
        let profile = ConnectionProfile(
            displayName: "test", host: "example.com", port: 22, username: "user",
            transportPreference: .tsshdQuic
        )
        let controller = try makeControllerWithProfile(profile)

        controller.connect()

        try await waitUntilFixtureCondition(timeout: 2) {
            await controller.uiState.state != .connecting
        }
        guard case .failed(let message) = controller.uiState.state else {
            XCTFail("expected .failed state, got \(controller.uiState.state)")
            return
        }
        XCTAssertTrue(message.contains("未対応"))
    }

    // MARK: - Phase 1C(#14): reconnect()

    func testReconnectAfterFailedStateRetriesConnect() async throws {
        let profile = ConnectionProfile(
            displayName: "test", host: "example.com", port: 22, username: "user",
            transportPreference: .tsshdQuic
        )
        let controller = try makeControllerWithProfile(profile)
        controller.connect()
        try await waitUntilFixtureCondition(timeout: 2) {
            await controller.uiState.state != .connecting
        }
        guard case .failed = controller.uiState.state else {
            XCTFail("expected initial .failed state, got \(controller.uiState.state)")
            return
        }

        controller.reconnect()

        // .tsshdQuicは常に同期的に失敗するため、reconnect()は.connectingを経由して
        // 再び.failedへ戻る(再接続の試行自体が行われたことを示す)。
        try await waitUntilFixtureCondition(timeout: 2) {
            await controller.uiState.state != .connecting
        }
        guard case .failed = controller.uiState.state else {
            XCTFail("expected .failed state after reconnect, got \(controller.uiState.state)")
            return
        }
    }

    func testReconnectWhileConnectingIsIgnored() throws {
        let profile = ConnectionProfile(displayName: "test", host: "example.com", port: 22, username: "user")
        let controller = try makeControllerWithProfile(profile)
        XCTAssertEqual(controller.uiState.state, .connecting)

        controller.reconnect()

        // .connecting中はreconnect()が無視されるため、状態が変わらないままである
        // (connect()が呼ばれ直していれば別のセッションが生成され得るが、ここでは
        // stateの遷移が起きないことだけを見て二重接続防止を検証する)。
        XCTAssertEqual(controller.uiState.state, .connecting)
    }

    // MARK: - Phase 1C(#25): trzszファイル転送

    func testOnTrzszStateChangedWaitingUserSetsWaitingUserState() async throws {
        let profile = ConnectionProfile(displayName: "test", host: "example.com", port: 22, username: "user")
        let controller = try makeControllerWithProfile(profile)

        controller.onTrzszStateChanged(
            state: .waitingUser(transferId: "t1", mode: "download", suggestedName: "report.txt", expectedSize: 1024)
        )

        try await waitUntilFixtureCondition(timeout: 2) {
            await controller.uiState.trzszState != nil
        }
        XCTAssertEqual(
            controller.uiState.trzszState,
            .waitingUser(transferId: "t1", mode: "download", suggestedName: "report.txt", expectedSize: 1024)
        )
    }

    func testOnTrzszStateChangedInProgressUpdatesTrzszState() async throws {
        let profile = ConnectionProfile(displayName: "test", host: "example.com", port: 22, username: "user")
        let controller = try makeControllerWithProfile(profile)

        controller.onTrzszStateChanged(
            state: .waitingUser(transferId: "t1", mode: "download", suggestedName: "report.txt", expectedSize: 1024)
        )
        controller.onTrzszStateChanged(
            state: .inProgress(transferId: "t1", mode: "download", fileName: "report.txt", transferred: 512, total: 1024)
        )

        try await waitUntilFixtureCondition(timeout: 2) {
            guard case .inProgress = await controller.uiState.trzszState else { return false }
            return true
        }
        XCTAssertEqual(
            controller.uiState.trzszState,
            .inProgress(transferId: "t1", mode: "download", fileName: "report.txt", transferred: 512, total: 1024)
        )
    }

    func testOnTrzszStateChangedDoneForUploadDoesNotSetCompletedDownloadURL() async throws {
        let profile = ConnectionProfile(displayName: "test", host: "example.com", port: 22, username: "user")
        let controller = try makeControllerWithProfile(profile)

        controller.onTrzszStateChanged(
            state: .waitingUser(transferId: "t1", mode: "upload", suggestedName: nil, expectedSize: nil)
        )
        controller.onTrzszStateChanged(state: .done(transferId: "t1", success: true, message: nil))

        try await waitUntilFixtureCondition(timeout: 2) {
            guard case .done = await controller.uiState.trzszState else { return false }
            return true
        }
        XCTAssertNil(controller.uiState.completedDownloadURL)
    }

    func testDownloadRequestWritesDataToTempFileAndExposesURLOnFinish() async throws {
        let profile = ConnectionProfile(displayName: "test", host: "example.com", port: 22, username: "user")
        let controller = try makeControllerWithProfile(profile)

        controller.onTrzszStateChanged(
            state: .waitingUser(transferId: "t1", mode: "download", suggestedName: "hello.txt", expectedSize: 5)
        )
        controller.trzszStartDownload()
        controller.onDownloadComplete(fileName: nil, data: Data("hello".utf8))
        controller.onTrzszStateChanged(state: .done(transferId: "t1", success: true, message: nil))

        try await waitUntilFixtureCondition(timeout: 2) {
            await controller.uiState.completedDownloadURL != nil
        }
        let url = try XCTUnwrap(controller.uiState.completedDownloadURL)
        XCTAssertEqual(try Data(contentsOf: url), Data("hello".utf8))

        controller.trzszDismiss()
        XCTAssertNil(controller.uiState.trzszState)
        XCTAssertNil(controller.uiState.completedDownloadURL)
        XCTAssertFalse(FileManager.default.fileExists(atPath: url.path))
    }

    func testTrzszDismissClearsStateWithoutCompletedDownload() async throws {
        let profile = ConnectionProfile(displayName: "test", host: "example.com", port: 22, username: "user")
        let controller = try makeControllerWithProfile(profile)

        controller.onTrzszStateChanged(
            state: .waitingUser(transferId: "t1", mode: "upload", suggestedName: nil, expectedSize: nil)
        )
        try await waitUntilFixtureCondition(timeout: 2) {
            await controller.uiState.trzszState != nil
        }

        controller.trzszDismiss()

        XCTAssertNil(controller.uiState.trzszState)
        XCTAssertNil(controller.uiState.completedDownloadURL)
    }

    // MARK: - タスク#26: BEL受信時の触覚フィードバック

    /// 他フィールドは`TerminalScreenViewTests`の既存テストと同じ最小値で埋め、
    /// `bellGeneration`だけを可変にする。
    private func makeScreenUpdate(bellGeneration: UInt64) -> ScreenUpdate {
        ScreenUpdate(
            updateSeq: 0, cols: 1, rows: 1, cells: [],
            cursorRow: 0, cursorCol: 0,
            title: nil, applicationCursorMode: false, applicationKeypadMode: false, bracketedPasteMode: false,
            mouseReportingMode: .off, sgrMouseMode: false,
            cursorVisible: true, bellGeneration: bellGeneration,
            cursorShape: .block, cursorBlink: true, linkTable: [], images: [], kittyKeyboardFlags: 0, dirtyRows: nil
        )
    }

    func testOnScreenUpdateAdvancesLastFiredBellGenerationWhenBellGenerationIncreases() async throws {
        let profile = ConnectionProfile(displayName: "test", host: "example.com", port: 22, username: "user")
        let controller = try makeControllerWithProfile(profile)
        XCTAssertEqual(controller.uiState.lastFiredBellGeneration, 0)

        controller.onScreenUpdate(update: makeScreenUpdate(bellGeneration: 1))
        try await waitUntilFixtureCondition(timeout: 2) {
            await controller.uiState.lastFiredBellGeneration == 1
        }

        controller.onScreenUpdate(update: makeScreenUpdate(bellGeneration: 3))
        try await waitUntilFixtureCondition(timeout: 2) {
            await controller.uiState.lastFiredBellGeneration == 3
        }
    }

    /// 同一`bellGeneration`の`ScreenUpdate`が再適用されても(conflatedチャネル越しの
    /// 重複配送等)`lastFiredBellGeneration`は変化しない(`ScreenUpdate.bellGeneration`の
    /// docコメント通り、二重フィードバックを避けるための`>`比較)。
    func testOnScreenUpdateDoesNotAdvanceLastFiredBellGenerationWhenSameValueReapplied() async throws {
        let profile = ConnectionProfile(displayName: "test", host: "example.com", port: 22, username: "user")
        let controller = try makeControllerWithProfile(profile)

        controller.onScreenUpdate(update: makeScreenUpdate(bellGeneration: 3))
        try await waitUntilFixtureCondition(timeout: 2) {
            await controller.uiState.lastFiredBellGeneration == 3
        }

        // 直接同期比較できないため(Task経由でMainActorへ反映される)、他のフィールドを
        // 変えたScreenUpdateを挟み、そのフィールドがuiStateへ反映されるのを待つことで
        // 直前のonScreenUpdate呼び出しがMainActorキュー上で処理済みであることを保証する。
        controller.onScreenUpdate(update: ScreenUpdate(
            updateSeq: 0, cols: 2, rows: 1, cells: [],
            cursorRow: 0, cursorCol: 0,
            title: nil, applicationCursorMode: false, applicationKeypadMode: false, bracketedPasteMode: false,
            mouseReportingMode: .off, sgrMouseMode: false,
            cursorVisible: true, bellGeneration: 3,
            cursorShape: .block, cursorBlink: true, linkTable: [], images: [], kittyKeyboardFlags: 0, dirtyRows: nil
        ))
        try await waitUntilFixtureCondition(timeout: 2) {
            await controller.uiState.latestScreenUpdate?.cols == 2
        }

        XCTAssertEqual(controller.uiState.lastFiredBellGeneration, 3)
    }

    /// Fableレビュー指摘: `reconnect()`(#14)経由で新しい論理セッションが始まると
    /// Rust側`bell_generation`は0から再スタートする(#24、Terminalごとの単調増加)ため、
    /// `lastFiredBellGeneration`の記憶も`reconnect()`が(`connect()`を呼ぶ直前に)明示的に
    /// 0へリセットしないと「新セッションのgen 1 < 旧セッションで記憶した値」で最初の
    /// BELを取りこぼす(Codexレビュー指摘: このリセットは`uiState.latestScreenUpdate = nil`
    /// と同じ@MainActor同期文脈で行う必要があり、`connect()`側でTask経由の非同期リセットに
    /// すると新セッション最初の`onScreenUpdate`と競合しうる)。
    func testReconnectResetsLastFiredBellGenerationForNewSession() async throws {
        // .tsshdQuicは常に同期的に失敗するtransport(iOS版未対応)なので、実接続なしに
        // `connect()`/`reconnect()`が呼ばれたことだけを検証できる
        // (`testReconnectAfterFailedStateRetriesConnect`と同じ手法)。
        let profile = ConnectionProfile(
            displayName: "test", host: "example.com", port: 22, username: "user",
            transportPreference: .tsshdQuic
        )
        let controller = try makeControllerWithProfile(profile)
        controller.connect()
        try await waitUntilFixtureCondition(timeout: 2) {
            await controller.uiState.state != .connecting
        }

        // 旧セッションで既にBELを記憶した状態を模す。
        controller.onScreenUpdate(update: makeScreenUpdate(bellGeneration: 5))
        try await waitUntilFixtureCondition(timeout: 2) {
            await controller.uiState.lastFiredBellGeneration == 5
        }

        controller.reconnect()

        try await waitUntilFixtureCondition(timeout: 2) {
            await controller.uiState.lastFiredBellGeneration == 0
        }
    }

    // MARK: - タスク#67: スクロールバック検索(search_scrollback)

    /// マッチ計算そのものは`SessionCore::search_scrollback`(Rust、#37)側で既にテスト済み
    /// (`session.rs`の`search_scrollback_*`群)。ここでは`TerminalSessionController
    /// .searchScrollback`がその呼び出しを中継しているだけであること(セッション未確立時に
    /// クラッシュせず空配列を返すこと)を確認する——`scrollbackCells`/`scrollbackLen`と
    /// 同じ「未接続ガード」の契約。
    func testSearchScrollbackReturnsEmptyBeforeConnecting() throws {
        let (controller, _) = try makeController()
        XCTAssertEqual(controller.searchScrollback(query: "abc", caseSensitive: false), [])
    }
}
