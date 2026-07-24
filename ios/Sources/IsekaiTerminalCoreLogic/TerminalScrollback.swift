import Foundation

/// Phase 1F-4(#51): ライブの`update`とスクロールバックの行から表示用の`ScreenUpdate`を
/// 合成する。Android版`tools.isekai.terminal.ui.synthesizeDisplayUpdate`
/// (`TerminalScrollback.kt`、`TerminalScreen.kt`の`displayUpdate`から呼ばれる)と対称
/// (タスク#46でAndroid側もこの合成ロジックを独立関数へ抽出した)。`scrollOffset == 0`かつ
/// `showingScrollback`が`false`のときだけライブをそのまま返す——`scrollOffset == 0`は
/// 「ライブ画面表示」と「scrollback最新行(row=0)表示」の両方を指しうるため、
/// `showingScrollback`で明示的に区別する(タスク#79: 検索結果のrow=0へジャンプする際、
/// `scrollOffset`を変えずにscrollback最新行を合成表示させるために追加。呼び出し元
/// `TerminalView.swift`の`showingScrollback`参照、Android版`TerminalScreen.kt`と対称)。
/// `scrollbackCells`の件数が`cols * rows`と一致しない場合(未取得・セッション未確立等)も
/// ライブへフォールバックする。
///
/// スクロールバック表示中はカーソルを画面外(`cursorRow = update.rows`)に隠す
/// (Android版と同じ、ライブでない行にカーソルを描くのは意味がないため)。
public func synthesizeDisplayUpdate(
    live update: ScreenUpdate,
    scrollOffset: UInt32,
    scrollbackCells: [CellData],
    showingScrollback: Bool = false
) -> ScreenUpdate {
    guard scrollOffset > 0 || showingScrollback else { return update }
    let cols = Int(update.cols)
    let rows = Int(update.rows)
    guard cols > 0, rows > 0, scrollbackCells.count == rows * cols else { return update }
    return ScreenUpdate(
        // updateSeqはライブの値をそのまま引き継ぐ(この合成はdraw経路の
        // `computeDisplayUpdate`内でのみ使われ、`apply`のupdateSeqギャップ検出は通らない
        // ——値は実質inertだが、他フィールド同様ライブを忠実にミラーしておく)。
        updateSeq: update.updateSeq,
        cols: update.cols, rows: update.rows, cells: scrollbackCells,
        cursorRow: update.rows, cursorCol: 0,
        title: update.title,
        applicationCursorMode: update.applicationCursorMode,
        applicationKeypadMode: update.applicationKeypadMode,
        bracketedPasteMode: update.bracketedPasteMode,
        mouseReportingMode: update.mouseReportingMode,
        sgrMouseMode: update.sgrMouseMode,
        alternateScroll: update.alternateScroll,
        urxvtMouseMode: update.urxvtMouseMode,
        cursorVisible: update.cursorVisible,
        bellGeneration: update.bellGeneration,
        cursorShape: update.cursorShape,
        cursorBlink: update.cursorBlink,
        linkTable: update.linkTable,
        // Sixel(タスク#42)/Kitty graphics(タスク#53): scrollback表示中はライブ画面の画像
        // 配置を引き継がない(scrollbackセル自体は画像を保持しないテキストのみの
        // スナップショットのため、Android版`synthesizeDisplayUpdate`と同じ判断)。
        images: [],
        kittyKeyboardFlags: update.kittyKeyboardFlags,
        // スクロールバック合成は表示グリッド全体を差し替えるため全画面dirty(=nil)扱い。
        // ライブの`update.dirtyRows`はライブグリッドの行番号を指しており、scrollback表示
        // 行とは対応しないので引き継がない(タスク#102)。
        dirtyRows: nil
    )
}

/// タスク#66/#67: 検索バーの現在マッチ(`match`)のうち、実際に`scrollOffset`の位置へ
/// ハイライトとして描画してよいものだけを返すピュア関数。Android版
/// `tools.isekai.terminal.searchHighlightMatch`(`TerminalScreen.kt`)と対称
/// (タスク#79でAndroid側に続きiOS側もこの判断ロジックを独立関数へ抽出し、UIKit非依存の
/// `IsekaiTerminalCoreLogic`層でユニットテスト可能にした——以前は`TerminalView.swift`の
/// `body`内にインライン記述されていたため、`TerminalScreenViewTests`のクラッシュ確認
/// スモークテストでしか間接的にしか検証できなかった)。
///
/// `ScrollbackSearchMatch.row`は`scrollbackCells`と同じ規約("offset"がそのまま`row`)なので、
/// `scrollOffset`がその値と一致している間だけ実際に画面へ表示される。`scrollOffset == 0`は
/// 「ライブ画面表示」と「scrollback最新行(row=0)表示」の両方を指しうる(既存規約
/// `synthesizeDisplayUpdate`と`showingScrollback`参照)ため、`row == 0`のマッチは
/// `showingScrollback`が真の間(=実際にscrollback最新行を表示中)だけハイライトを許可する
/// (タスク#79: それ以外[ライブ画面表示中]にrow=0のマッチを誤ってハイライトしないための
/// ガード)。
public func searchHighlightMatch(
    _ match: ScrollbackSearchMatch?,
    scrollOffset: UInt32,
    showingScrollback: Bool
) -> ScrollbackSearchMatch? {
    guard let match, scrollOffset == match.row, match.row != 0 || showingScrollback else { return nil }
    return match
}
