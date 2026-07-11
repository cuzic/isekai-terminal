import Foundation
import IsekaiTerminalCoreLogic

/// tmux迂回control-plane(russhのstreamlocal forward経由でリモートの
/// `isekai-pipe ctl title|clip push`を直接受け取る、`ISEKAI_PIPE_DESIGN.md` §8 Epic M)の
/// 有効/無効を、Rust側のプロセスグローバル状態(`setCtlSocketForwardEnabled`)へ反映する。
/// Android版`MainActivity.kt`の`restoreCtlSocketForwardEnabled`/
/// `restorePersistedCtlSocketForward`に相当する。
public enum CtlSocketForwardSettings {
    /// アプリ起動時に一度呼ぶ(`AppRootView.init()`)。トグル変更時は`ProfileListView`が
    /// 都度これを呼び直す。
    public static func restore(defaults: UserDefaults = .standard) {
        setCtlSocketForwardEnabled(enabled: defaults.bool(forKey: AppSettingsKeys.enableCtlSocketForward))
    }
}
