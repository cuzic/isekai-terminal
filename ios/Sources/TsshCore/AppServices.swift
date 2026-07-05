import Foundation

/// Phase 1D: アプリ全体で共有するローカルDB/Vaultのシングルトン置き場。
/// Android版`data.Repositories`(DAO/リポジトリのシングルトン集約)に相当する。
/// テストからは触れず、実アプリ(`TsshTerminalApp`)からのみ使う想定。
public final class AppServices {
    public static let shared = AppServices()

    public let db: ProfileDatabase
    public let vault: CredentialVault
    public let trustStore: SshHostTrustStore
    public let relayVault = RelayCredentialVault()

    private init() {
        let support = FileManager.default.urls(for: .applicationSupportDirectory, in: .userDomainMask)[0]
        do {
            try FileManager.default.createDirectory(at: support, withIntermediateDirectories: true)
            db = try ProfileDatabase(path: support.appendingPathComponent("tssh_terminal.sqlite").path)
            vault = try CredentialVault(blobDirectory: support.appendingPathComponent("credential_vault", isDirectory: true))
            trustStore = try SshHostTrustStore(storeURL: support.appendingPathComponent("ssh_host_trust.json"))
        } catch {
            fatalError("AppServices initialization failed: \(error)")
        }
    }
}
