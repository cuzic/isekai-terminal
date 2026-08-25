import Foundation

/// Y-P1(#5): OSC 133(タスク#13)「前/次のプロンプトへジャンプ」の結果から、
/// `TerminalView`が持つ`scrollOffset`/`showingScrollback`(UI表示だけに閉じた状態、
/// `.claude/rules/rust-ssot.md`の例外)へどう反映すべきかを決める純関数。
/// Android版`TerminalScreen.kt`の`LaunchedEffect(uiState.promptJumpResult.seq)`内の
/// 分岐(`ADR_IOS_PARITY_IMPLEMENTATION.md` §3.1)をLogic層(Linux CIで検証可能)へ
/// 抽出したもの。`PromptJumpTarget.isLive`が「`scrollOffset == 0`はライブ画面表示」
/// という規約と「scrollback最新行(row=0)表示」を明示的に区別する(タスク#79と同じ理由)。
public enum PromptNavigation {
    public struct ScrollTarget: Equatable {
        public let scrollOffset: UInt32
        public let showingScrollback: Bool

        public init(scrollOffset: UInt32, showingScrollback: Bool) {
            self.scrollOffset = scrollOffset
            self.showingScrollback = showingScrollback
        }
    }

    /// `target`が`nil`(該当プロンプトなし)の場合は`nil`を返す
    /// (呼び出し元は`notFoundMessage()`を使って表示文言を決める)。
    public static func scrollTarget(for target: PromptJumpTarget?) -> ScrollTarget? {
        guard let target else { return nil }
        return target.isLive
            ? ScrollTarget(scrollOffset: 0, showingScrollback: false)
            : ScrollTarget(scrollOffset: target.scrollOffset, showingScrollback: true)
    }

    /// ジャンプ対象が見つからなかった場合の表示文言。Android版は視覚フィードバックを
    /// 出さない(将来スコープ)が、iOSでは軽い案内を表示する(D-3: Androidが解決していた
    /// 困りごと自体は「ジャンプ先が無いことが分かる」ことなので、iOSの語彙として
    /// この程度のフィードバックを追加しても不整合ではないという判断)。
    /// `target`が非nilの場合は`nil`を返す(=何も表示しない)。
    public static func notFoundMessage(for target: PromptJumpTarget?) -> String? {
        target == nil ? "前後にジャンプ可能なプロンプトが見つかりませんでした" : nil
    }
}
