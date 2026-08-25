import Foundation

/// `UserDefaults.standard`に永続化するアプリ全体(プロファイル単位ではない)設定のキー。
/// Android版`MainActivity.kt`の`PREF_KEY_*`定数と同じ文字列を踏襲する(値の意味を
/// 揃えるための対応付けであり、Android/iOS間でストレージを共有しているわけではない)。
public enum AppSettingsKeys {
    /// 画面の保護(Android版`FLAG_SECURE`相当、`ScreenProtectionOverlay`参照)。既定OFF。
    public static let screenProtectionEnabled = "screen_protection_enabled"
    /// リモートからのOSC 52クリップボード書き込みを許可するか(`RemoteClipboardBridge`参照)。既定OFF。
    public static let allowRemoteClipboardWrite = "allow_remote_clipboard_write"
    /// リモートからのOSC 52クリップボード読み出し(query)に応答するか(`RemoteClipboardBridge`参照)。既定OFF。
    public static let allowRemoteClipboardPull = "allow_remote_clipboard_pull"
    /// tmux迂回control-plane(`CtlSocketForwardSettings`参照)を有効にするか。既定OFF。
    public static let enableCtlSocketForward = "enable_ctl_socket_forward"
    /// Y-P1(#7): 新規(未知)ホスト鍵を確認ダイアログ無しで自動信頼するか
    /// (`ssh -o StrictHostKeyChecking=accept-new`相当、`TerminalSessionController.onHostKey`参照)。
    /// 既知ホストのfingerprint不一致はこの設定に関わらず常に拒否する(`always-connects.md`)。
    /// 既定OFF。Android版`HostKeySettings`の`auto_trust_new_host_keys`と同じ意味。
    public static let autoTrustNewHostKeys = "auto_trust_new_host_keys"
}
