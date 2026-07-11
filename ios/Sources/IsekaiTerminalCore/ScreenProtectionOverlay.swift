import SwiftUI

/// Android版`applyScreenProtection`(`WindowManager.LayoutParams.FLAG_SECURE`)に相当する
/// 「画面の保護」。iOSにはスクリーンショット自体をブロックするpublic APIが存在しないため
/// 完全な同等機能ではないが、Android版が同時に満たしていた「最近使ったアプリ」の
/// サムネイルへ実内容が写り込むのを防ぐ部分は、バックグラウンド遷移時に不透明な
/// カバーへ即座に差し替えることで実現できる(多くの銀行アプリ等が使う標準的な手法)。
/// スクリーンショット自体・画面録画中の映り込みは防げないため、その旨をユーザーに
/// 説明する文言は`ProfileListView`のメニュー側で扱う想定。
public struct ScreenProtectionOverlay: ViewModifier {
    @AppStorage(AppSettingsKeys.screenProtectionEnabled) private var isEnabled = false
    @Environment(\.scenePhase) private var scenePhase

    public init() {}

    public func body(content: Content) -> some View {
        content.overlay {
            if isEnabled && scenePhase != .active {
                ZStack {
                    Color(.systemBackground)
                    Image(systemName: "lock.shield")
                        .font(.system(size: 48))
                        .foregroundStyle(.secondary)
                }
                .ignoresSafeArea()
                .accessibilityIdentifier("screenProtectionOverlay")
                .transition(.identity)
            }
        }
    }
}

extension View {
    /// アプリのルートView(`AppRootView`)に一度だけ適用する。
    public func screenProtected() -> some View {
        modifier(ScreenProtectionOverlay())
    }
}
