package tools.isekai.terminal.ui

import uniffi.isekai_terminal_core.MouseButton
import uniffi.isekai_terminal_core.MouseReportingMode

/**
 * タスク#87: `TerminalScreen.kt`の`pointerInput`コルーチンに直書きされていたマウスUI裁定
 * ロジック(press/drag/releaseのライフサイクル・2本指中断→ピンチ引き継ぎ[タスク#80]・
 * scrollOffsetゲート・wheel経路のボタン判定)をピュア関数として抽出したもの。
 *
 * これらの関数自体は`AwaitPointerEventScope`/`PointerInputChange`等のCompose実行時型に
 * 依存せず、呼び出し側([TerminalScreen.kt]のジェスチャーループ)がその都度の状態
 * (押されている指の本数・追跡中の指が押されたままか等)をプリミティブ値として渡す。
 * これにより、実際にCompose実行環境(Robolectric)なしのプレーンJUnitで裁定ロジック
 * 自体の回帰テストが書ける([MouseGestureArbiterTest]参照)。iOS版
 * `TerminalScreenView.swift`の`isPointerReportingActive`/`touchesBegan`と対称の判断。
 */

/**
 * マウスレポーティング(`?1000`/`?1002`/`?1003`)が実際に有効か。モードがOFFでない
 * ことに加え、スクロールバック表示中(`scrollOffset > 0`、またはタスク#79の
 * `showingScrollback`)は対象外とする(表示対象[過去ログ]と入力対象[ライブ
 * セッション]が食い違うのを避ける)。iOS版`isPointerReportingActive`と対称。
 */
fun isPointerReportingActive(
    scrollOffset: Int,
    showingScrollback: Boolean,
    mouseReportingMode: MouseReportingMode,
): Boolean = scrollOffset == 0 && !showingScrollback && mouseReportingMode != MouseReportingMode.OFF

/**
 * ジェスチャー開始時点で、単一指のタッチを選択/スクロールバックパンではなく
 * マウスのpress/drag/releaseとして扱うべきか。マウスレポーティングが有効でも、
 * 開始時点で既に2本以上の指が押されている場合はマウスタッチ経路を使わず
 * (ピンチ優先)、通常のジェスチャー裁定([classifyNormalGesture])へ渡す。
 */
fun shouldUseMouseTouch(pointerReportingActive: Boolean, initialPointerCount: Int): Boolean =
    pointerReportingActive && initialPointerCount <= 1

/** マウスタッチ追跡中に届いた1つのpointer eventに対して取るべきアクション。 */
enum class MouseTouchStep {
    /** 引き続き同じ指を追跡し、MOTIONを送る。 */
    CONTINUE,

    /** 追跡中の指が離れた(2本目以降は触れていない)。RELEASEを送って追跡終了する。 */
    RELEASE_ONLY,

    /**
     * 2本目以降の指が触れてきた(追跡中の指自体が離れたかどうかは問わない)。
     * これ以上単一指のドラッグとしては扱えないため、直前のpressに対応するreleaseを
     * 送って打ち切り、同じジェスチャをそのままピンチ/パン処理へ引き継ぐ(タスク#80:
     * 以前はreleaseを送って中断するだけでピンチへ継続できず、マウスモード有効時は
     * ピンチが実質使えなかった)。
     */
    RELEASE_AND_HANDOFF_TO_PINCH,
}

/**
 * [trackedFingerPressed]: 追跡中の指(最初にpressしたポインタid)が、このイベント
 * 時点でもまだ押されているか。[pointerCount]: このイベント時点で押されている
 * 指の総数(追跡中の指を含む)。
 */
fun decideMouseTouchStep(trackedFingerPressed: Boolean, pointerCount: Int): MouseTouchStep = when {
    pointerCount > 1 -> MouseTouchStep.RELEASE_AND_HANDOFF_TO_PINCH
    !trackedFingerPressed -> MouseTouchStep.RELEASE_ONLY
    else -> MouseTouchStep.CONTINUE
}

/** マウスタッチ経路を使わない通常ジェスチャーが、最終的にどう分類されるか。 */
enum class NormalGestureOutcome {
    /** 長押し成立(かつ2本指以上ではない) → 選択モード。 */
    SELECTION,

    /** 長押し不成立かつ移動なしで指が離れた(かつ2本指以上ではない) → 単純タップ。 */
    TAP,

    /** 2本指以上、または長押し不成立で移動あり → ピンチ拡縮+縦パンスクロール。 */
    PINCH_PAN,
}

/**
 * [longPressSucceeded]: `awaitLongPressOrCancellation`が非nullを返したか。
 * [pointerCount]: 判定時点で押されている指の総数。[trackedFingerStillPressed]:
 * 最初にdownした指(`down.id`)が、判定時点でもまだ押されているか(既に指が
 * 離れて`changes`から消えている場合も`false`として渡す)。
 */
fun classifyNormalGesture(
    longPressSucceeded: Boolean,
    pointerCount: Int,
    trackedFingerStillPressed: Boolean,
): NormalGestureOutcome = when {
    longPressSucceeded && pointerCount < 2 -> NormalGestureOutcome.SELECTION
    pointerCount < 2 && !trackedFingerStillPressed -> NormalGestureOutcome.TAP
    else -> NormalGestureOutcome.PINCH_PAN
}

/**
 * タスク#88(fableレビュー・グループD指摘): xtermは同一セル内でのマウス移動を
 * 重複報告しないが、Composeの`awaitPointerEvent`は最大120Hz程度で発火しうるため、
 * ドラッグ中は指がわずかに揺れただけでも同じセルへ何度もMOTIONイベントを
 * SSHへ送ってしまっていた。呼び出し側([TerminalScreen.kt]のドラッグループ)が
 * 直近に実際に送信したセル座標(`lastReportedCell`)を1変数として保持し、新しい
 * セル座標([newCell])と比較する。座標が変わった場合のみ`true`(送信すべき)を返す。
 * iOS版`TerminalScreenView.swift`の`shouldReportMouseMotion`と対称。
 */
fun shouldReportMouseMotion(lastReportedCell: CellPos, newCell: CellPos): Boolean =
    newCell != lastReportedCell

/**
 * トラックパッド/マウスホイールの`PointerEventType.Scroll`の縦方向delta量から、
 * 送出すべきxtermホイールボタンを決める。`deltaY == 0f`(スクロール量なし)は
 * 対象外として`null`を返す。符号規約はComposeのスクロール系APIと同じ:
 * 正のdeltaY = コンテンツを上へ送る(=下方向へスクロール、xtermのwheel down/
 * button 65)。
 */
fun wheelButtonForDelta(deltaY: Float): MouseButton? = when {
    deltaY == 0f -> null
    deltaY > 0f -> MouseButton.WHEEL_DOWN
    else -> MouseButton.WHEEL_UP
}

/**
 * 補助操作ドロワー(キーボード表示・ローカル履歴ページ送り・PgUp/PgDn・マウスホイール
 * 送信のアイコンを一時表示するUI)を表示すべきか。指の本数を問わず、ジェスチャー
 * 開始位置から上方向へ[thresholdPx]以上動いたら表示する。マウスレポーティング有効時に
 * 1本指タップがクリックとして奪われる問題([shouldUseMouseTouch]参照)の回避策として、
 * どの経路(マウスタッチ転送/選択/ピンチ)が同時に処理されていても検出できるよう、
 * 呼び出し側では他のジェスチャーを消費しない別系統の`pointerInput`から呼ぶ想定
 * (どのタッチ経路が同時に走っていても検出できる)。
 */
fun shouldRevealAuxDrawer(startY: Float, currentY: Float, thresholdPx: Float): Boolean =
    startY - currentY >= thresholdPx

/**
 * トラックパッド/マウスホイールの`PointerEventType.Scroll`の横方向delta量から、
 * 送出すべきxtermホイールボタンを決める。`deltaX == 0f`(スクロール量なし)は
 * 対象外として`null`を返す。符号規約: 正のdeltaX = 右方向へスクロール
 * (xtermのwheel right/button 67)。
 */
fun wheelButtonForHorizontalDelta(deltaX: Float): MouseButton? = when {
    deltaX == 0f -> null
    deltaX > 0f -> MouseButton.WHEEL_RIGHT
    else -> MouseButton.WHEEL_LEFT
}
