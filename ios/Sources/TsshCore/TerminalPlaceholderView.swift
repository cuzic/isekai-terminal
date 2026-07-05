import SwiftUI

/// Phase 1D: ターミナル本画面(SSH接続 + レンダリング + IME統合)はまだ実装されていない。
/// プロファイル一覧から接続をタップした際の遷移先を確保しつつ、未実装であることを
/// 明示するプレースホルダー。実装され次第このViewは置き換える。
public struct TerminalPlaceholderView: View {
    private let profile: ConnectionProfile

    public init(profile: ConnectionProfile) {
        self.profile = profile
    }

    public var body: some View {
        VStack(spacing: 12) {
            Image(systemName: "terminal")
                .font(.system(size: 40))
                .foregroundStyle(.secondary)
            Text("\(profile.username)@\(profile.host):\(profile.port)")
                .font(.system(.body, design: .monospaced))
            Text("ターミナル画面は未実装です")
                .foregroundStyle(.secondary)
        }
        .accessibilityIdentifier("terminalPlaceholder")
        .navigationTitle(profile.displayName)
    }
}
