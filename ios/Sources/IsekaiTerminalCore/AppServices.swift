import Foundation
import os
import IsekaiTerminalCoreLogic

/// Phase 1D: アプリ全体で共有するローカルDB/Vaultのシングルトン置き場。
/// Android版`data.Repositories`(DAO/リポジトリのシングルトン集約)に相当する。
/// テストからは触れず、実アプリ(`IsekaiTerminalApp`)からのみ使う想定。
public final class AppServices {
    public static let shared = AppServices()
    private static let logger = Logger(subsystem: "tools.isekai.terminal", category: "app-services")

    public let db: ProfileDatabase
    public let vault: CredentialVault
    public let trustStore: SshHostTrustStore
    public let relayVault = RelayCredentialVault()
    /// タスク#3: Android版`ClientIdentity.getOrCreate(context)`が読む
    /// `SharedPreferences("isekai_terminal_ui")`と対称の永続ストア。
    public let clientIdentityStore: ClientIdentityStore = UserDefaultsClientIdentityStore()

    private init() {
        let support = FileManager.default.urls(for: .applicationSupportDirectory, in: .userDomainMask)[0]
        do {
            try FileManager.default.createDirectory(at: support, withIntermediateDirectories: true)
            db = try ProfileDatabase(path: support.appendingPathComponent("isekai_terminal.sqlite").path)
            vault = try CredentialVault(blobDirectory: support.appendingPathComponent("credential_vault", isDirectory: true))
            // 信頼ストアのJSONが壊れていても起動不能にしない(壊れたファイルは退避して
            // 空で開く、`SshHostTrustStore.openRecoveringCorruption`参照)。以前は
            // ここがthrowしてfatalErrorになり、再インストール以外に復旧手段が無かった。
            let opened = SshHostTrustStore.openRecoveringCorruption(
                storeURL: support.appendingPathComponent("ssh_host_trust.json")
            )
            trustStore = opened.store
            if let recovered = opened.recovered {
                Self.logger.error(
                    "ssh_host_trust.json was unreadable and has been reset (quarantined to \(recovered.quarantinedURL?.path ?? "<not moved>", privacy: .public)): \(String(describing: recovered.error), privacy: .public)"
                )
            }
        } catch {
            fatalError("AppServices initialization failed: \(error)")
        }
    }
}
