import Foundation
import IsekaiTerminalCoreLogic

/// タスク#3(Android版タスク#60`util/ClientIdentity.kt`のiOS移植)。
///
/// `UserDefaults`はAndroid版`SharedPreferences("isekai_terminal_ui")`の直接的な
/// 対応物としてこのコードベースで既に広く使われている(`TerminalThemes.swift`の
/// コメント、`AppSettingsKeys.swift`等参照)ため、新しい永続化機構は導入せずこれを使う。
/// `IsekaiTerminalCoreLogic`の`ClientIdentityStore`プロトコルへ薄く適合させるだけで、
/// 「値が無ければ生成して書き込む」という判断ロジック自体は`ClientIdentity.getOrCreate`
/// (`TmuxTabWindowCoordinator.swift`)側に置く。
public final class UserDefaultsClientIdentityStore: ClientIdentityStore {
    private let defaults: UserDefaults
    private static let key = "tmux_client_id"

    public init(defaults: UserDefaults = .standard) {
        self.defaults = defaults
    }

    public func readClientId() -> String? {
        defaults.string(forKey: Self.key)
    }

    public func writeClientId(_ value: String) {
        defaults.set(value, forKey: Self.key)
    }
}
