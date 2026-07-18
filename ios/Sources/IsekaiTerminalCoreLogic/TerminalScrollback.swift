import Foundation

/// Phase 1F-4(#51): ライブの`update`とスクロールバックの行から表示用の`ScreenUpdate`を
/// 合成する。Android版`tools.isekai.terminal.ui.synthesizeDisplayUpdate`
/// (`TerminalScrollback.kt`、`TerminalScreen.kt`の`displayUpdate`から呼ばれる)と対称
/// (タスク#46でAndroid側もこの合成ロジックを独立関数へ抽出した)。`scrollOffset == 0`なら
/// ライブをそのまま返す。`scrollbackCells`の件数が`cols * rows`と一致しない場合
/// (未取得・セッション未確立等)もライブへフォールバックする。
///
/// スクロールバック表示中はカーソルを画面外(`cursorRow = update.rows`)に隠す
/// (Android版と同じ、ライブでない行にカーソルを描くのは意味がないため)。
public func synthesizeDisplayUpdate(live update: ScreenUpdate, scrollOffset: UInt32, scrollbackCells: [CellData]) -> ScreenUpdate {
    guard scrollOffset > 0 else { return update }
    let cols = Int(update.cols)
    let rows = Int(update.rows)
    guard cols > 0, rows > 0, scrollbackCells.count == rows * cols else { return update }
    return ScreenUpdate(
        cols: update.cols, rows: update.rows, cells: scrollbackCells,
        cursorRow: update.rows, cursorCol: 0,
        title: update.title,
        applicationCursorMode: update.applicationCursorMode,
        bracketedPasteMode: update.bracketedPasteMode,
        mouseReportingMode: update.mouseReportingMode,
        sgrMouseMode: update.sgrMouseMode,
        cursorVisible: update.cursorVisible,
        bellGeneration: update.bellGeneration,
        cursorShape: update.cursorShape,
        cursorBlink: update.cursorBlink,
        linkTable: update.linkTable,
        // Sixel(タスク#42)/Kitty graphics(タスク#53): scrollback表示中はライブ画面の画像
        // 配置を引き継がない(scrollbackセル自体は画像を保持しないテキストのみの
        // スナップショットのため、Android版`synthesizeDisplayUpdate`と同じ判断)。
        images: [],
        kittyKeyboardFlags: update.kittyKeyboardFlags
    )
}
