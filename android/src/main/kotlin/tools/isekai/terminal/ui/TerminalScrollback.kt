package tools.isekai.terminal.ui

import uniffi.isekai_terminal_core.CellData
import uniffi.isekai_terminal_core.ScreenUpdate

/**
 * タスク#46: ライブの[live]とスクロールバックの行([scrollbackCells])から表示用の
 * [ScreenUpdate]を合成する。iOS版`TerminalScrollback.swift`の`synthesizeDisplayUpdate`と
 * 対称(このファイルはその移植元)。`scrollOffset <= 0`ならライブをそのまま返す。
 * [scrollbackCells]の件数が`live.cols * live.rows`と一致しない場合(未取得・セッション未確立・
 * リサイズ中の過渡状態等)もライブへフォールバックする。
 *
 * 検証対象の寸法は必ず[live]自身の`cols`/`rows`から導出すること(呼び出し側でCompose層が
 * 独自に計算したビューポート由来のcols/rowsを渡してはいけない)。`actions.onScrollbackCells`
 * へのリクエストも同じ[live]の寸法で行うべき(呼び出し元`TerminalScreen.kt`参照)——
 * Rust側`SessionCore.resize()`はチャネル送出成功時点で`screen_cols`を即時更新する一方、
 * `live`(直近の`ScreenUpdate`ブロードキャスト)側は非同期に遅れて追随するため、
 * Compose層が独自計算したcols/rowsとRust側の実際のスクロールバック行幅が過渡的に
 * 食い違いうる(Codexレビュー: タスク#46)。両方を[live]基準に揃えることで、
 * 「検証は通ったが返す`ScreenUpdate.cols/rows`と実際の`cells`サイズが食い違う」
 * (`SshTerminalCanvas`側のインデックス計算がずれ、最悪`IndexOutOfBoundsException`になる)
 * という事故を構造的に防ぐ。
 *
 * `ScreenUpdate`にフィールドを追加すると、この関数を含む位置引数コンストラクタ呼び出しは
 * 必ずコンパイルエラーになる(UniFFI生成型はデフォルト引数を持たない)ため「更新忘れ」自体は
 * コンパイラが防ぐ。本当に注意が必要なのは**意味論**: 新フィールドをここでライブから
 * そのまま引き継ぐべきか、blank値にすべきかを都度判断すること。例えば`bellGeneration`は
 * BEL通知用のRust側SSOTカウンタ(タスク#24)なので、スクロールバック表示中も直近のライブ値を
 * 落としてはいけない(=引き継ぐ)。一方`images`(Sixel/Kittyタスク#42・#53)はscrollbackセル
 * 自体が画像を保持しないテキストのみのスナップショットのため、意図的に空にする。
 */
fun synthesizeDisplayUpdate(
    live: ScreenUpdate,
    scrollOffset: Int,
    scrollbackCells: List<CellData>?,
): ScreenUpdate {
    if (scrollOffset <= 0) return live
    val cols = live.cols.toInt()
    val rows = live.rows.toInt()
    // iOS版と同じくcols/rowsが0(未初期化画面等)の縮退ケースも明示的にライブへ
    // フォールバックする(`rows * cols == 0`のとき空のscrollbackCellsが偶然一致してしまう
    // のを避ける)。
    if (cols <= 0 || rows <= 0) return live
    if (scrollbackCells == null || scrollbackCells.size != rows * cols) return live
    return ScreenUpdate(
        cols = live.cols,
        rows = live.rows,
        cells = scrollbackCells,
        cursorRow = live.rows, // カーソルは画面外に隠す(ライブでない行に描く意味が無いため)
        cursorCol = 0u,
        title = live.title,
        applicationCursorMode = live.applicationCursorMode,
        applicationKeypadMode = live.applicationKeypadMode,
        bracketedPasteMode = live.bracketedPasteMode,
        mouseReportingMode = live.mouseReportingMode,
        sgrMouseMode = live.sgrMouseMode,
        cursorVisible = live.cursorVisible,
        bellGeneration = live.bellGeneration,
        cursorShape = live.cursorShape,
        cursorBlink = live.cursorBlink,
        linkTable = live.linkTable,
        // Sixel(タスク#42)/Kitty graphics(タスク#53): scrollback表示中はライブ画面の
        // 画像配置を引き継がない(scrollbackセル自体は画像を保持しないテキストのみの
        // スナップショットのため、cursorVisible相当の考え方で画像も非表示にする)。
        images = emptyList(),
        kittyKeyboardFlags = live.kittyKeyboardFlags,
    )
}
