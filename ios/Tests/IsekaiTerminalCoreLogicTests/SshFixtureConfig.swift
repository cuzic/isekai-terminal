import Foundation

/// `rust-core/scripts/ios-fixture/start-sshd-fixture.sh`が書き出す接続情報。
/// `SshVerticalSliceTests`/`KeyManagerTests`等、実sshdへの接続を伴うテストで共有する。
struct SshFixtureConfig: Decodable {
    static let defaultPath = "/tmp/ios-fixture/fixture.json"

    let host: String
    let port: Int
    let user: String
    let privateKeyPath: String
    let hostKeyFingerprint: String

    enum CodingKeys: String, CodingKey {
        case host, port, user
        case privateKeyPath = "private_key_path"
        case hostKeyFingerprint = "host_key_fingerprint"
    }

    /// `authorized_keys`は`privateKeyPath`と同じディレクトリに置かれる
    /// (`start-sshd-fixture.sh`参照)。sshdは接続の都度このファイルを読むため、
    /// 起動後に追記しても再起動不要で反映される。
    var authorizedKeysPath: String {
        URL(fileURLWithPath: privateKeyPath)
            .deletingLastPathComponent()
            .appendingPathComponent("authorized_keys")
            .path
    }

    static func load() throws -> SshFixtureConfig {
        let path = ProcessInfo.processInfo.environment["IOS_SSH_FIXTURE_JSON"] ?? defaultPath
        let data = try Data(contentsOf: URL(fileURLWithPath: path))
        return try JSONDecoder().decode(SshFixtureConfig.self, from: data)
    }
}

/// 簡易ポーリングヘルパー。条件が満たされるまで待つ(タイムアウトでXCTFail)。
func waitUntilFixtureCondition(timeout: TimeInterval, condition: () async -> Bool) async throws {
    let deadline = Date().addingTimeInterval(timeout)
    while Date() < deadline {
        if await condition() { return }
        try await Task.sleep(nanoseconds: 50_000_000) // 50ms
    }
    throw SshFixtureConditionTimeoutError()
}

struct SshFixtureConditionTimeoutError: Error {}
