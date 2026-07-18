import Foundation

/// Phase 1F-4(#51): ライブの`update`とスクロールバックの行から表示用の`ScreenUpdate`を
/// 合成する。Android版`TerminalScreen.kt`の`displayUpdate`(`remember(scrollOffset, rows,
/// update)`)と対称。`scrollOffset == 0`ならライブをそのまま返す。`scrollbackCells`の件数が
/// `cols * rows`と一致しない場合(未取得・セッション未確立等)もライブへフォールバックする。
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
        cursorVisible: update.cursorVisible,
        bellGeneration: update.bellGeneration
    )
}
