import SwiftUI

/// Y-P0(a): `ProfileListView.swift`にインラインされていた4つのオプトイン設定トグル
/// (画面の保護/リモートクリップボード書込・送信許可/tmux迂回control-plane)を、
/// 独立した設定画面へ抽出したもの(`ADR_IOS_PARITY_IMPLEMENTATION.md` §3.11(a))。
///
/// 今後 #7(ホスト鍵自動信頼トグル、Y-P1)・#3(フォントインポート入口、Y-P4)・
/// #6-Outcome-3(外部キーボード配列ピッカー、Y-P4)・#9(`BackgroundBehaviorView`入口、Y-P3)
/// がここへ入口を追加する予定。このタスク自体は既存4トグルの移設のみで新機能は足さない。
public struct SettingsView: View {
    @AppStorage(AppSettingsKeys.screenProtectionEnabled) private var screenProtectionEnabled = false
    @AppStorage(AppSettingsKeys.allowRemoteClipboardWrite) private var remoteClipboardWriteEnabled = false
    @AppStorage(AppSettingsKeys.allowRemoteClipboardPull) private var remoteClipboardPullEnabled = false
    @AppStorage(AppSettingsKeys.enableCtlSocketForward) private var ctlSocketForwardEnabled = false

    @Environment(\.dismiss) private var dismiss

    public init() {}

    public var body: some View {
        NavigationStack {
            List {
                Section {
                    Toggle("画面の保護", isOn: $screenProtectionEnabled)
                        .accessibilityIdentifier("screenProtectionToggle")
                } footer: {
                    Text("アプリ切り替え画面でターミナル内容をぼかします。")
                }
                Section {
                    Toggle("リモートからのクリップボード書込", isOn: $remoteClipboardWriteEnabled)
                        .accessibilityIdentifier("remoteClipboardWriteToggle")
                    Toggle("リモートへのクリップボード送信", isOn: $remoteClipboardPullEnabled)
                        .accessibilityIdentifier("remoteClipboardPullToggle")
                } header: {
                    Text("リモートクリップボード連携")
                } footer: {
                    Text("tmux control-plane経由でリモートホストとクリップボードを同期します。")
                }
                Section {
                    Toggle("tmux迂回control-plane", isOn: $ctlSocketForwardEnabled)
                        .accessibilityIdentifier("ctlSocketForwardToggle")
                        .onChange(of: ctlSocketForwardEnabled) { _ in
                            CtlSocketForwardSettings.restore()
                        }
                } footer: {
                    Text("ssh ctl-socket経由でtmuxセッションのタイトル/クリップボードを転送します。")
                }
            }
            .navigationTitle("設定")
            .accessibilityIdentifier("settingsView")
            .toolbar {
                ToolbarItem(placement: .cancellationAction) {
                    Button("閉じる") { dismiss() }
                        .accessibilityIdentifier("settingsDoneButton")
                }
            }
        }
    }
}
