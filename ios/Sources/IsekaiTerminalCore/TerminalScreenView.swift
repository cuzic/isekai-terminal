import UIKit
import IsekaiTerminalCoreLogic

/// Phase 1F-2(#49): ピンチズームでのフォント拡縮率のクランプ計算(0.5〜3.0)。
/// Android版`fontScale.coerceIn(0.5f, 3.0f)`と対称。UIKitのジェスチャコールバックから
/// 分離してあるためテスト容易(ネットワーク/UIに触れない純粋関数)。
func clampedFontScale(current: CGFloat, zoomDelta: CGFloat) -> CGFloat {
    min(max(current * zoomDelta, 0.5), 3.0)
}

/// タスク#81: トラックパッド/マウスホイールの間接スクロール(`UIPanGestureRecognizer`が
/// `numberOfTouches == 0`で報告する連続的な`translation`)から、送出すべき
/// `MouseButton`(`.wheelUp`/`.wheelDown`)の並びと次回呼び出しへ持ち越す端数(`carry`)を
/// 求める純粋関数。Android版`PointerEventType.Scroll`経路(タスク#50)は1イベント=1notchで
/// 届くのに対し、iOSの間接scrollは指のタッチスワイプと同じ連続translationとして届くため、
/// セル1行分の移動量が溜まるたびに1回ホイールイベントとして切り出す(タッチスワイプでの
/// `scrollOffset`蓄積ループ`handlePan`と同じ考え方)。UIKit依存を持たない純粋関数として
/// 切り出してあるためユニットテストで直接検証できる(`clampedFontScale`と同様の方針)。
func wheelEvents(deltaY: CGFloat, carry: CGFloat, cellHeight: CGFloat) -> (buttons: [MouseButton], carry: CGFloat) {
    guard cellHeight > 0 else { return ([], carry) }
    var accum = carry + deltaY
    var buttons: [MouseButton] = []
    // Android版と同じ符号規約(既存`handlePan`のscrollOffset蓄積ループを参照): 負方向
    // (画面/コンテンツが上へ動く=履歴を遡る)は`scrollOffset`を増やす操作と同じ向きなので
    // xtermの"wheel up"(button 64、古い内容を見せる方向)に対応させる。正方向はその逆で
    // "wheel down"(button 65)。
    while accum < -cellHeight {
        buttons.append(.wheelUp)
        accum += cellHeight
    }
    while accum > cellHeight {
        buttons.append(.wheelDown)
        accum -= cellHeight
    }
    return (buttons, accum)
}

/// タスク#86: blink位相(`blinkPhaseVisible`)をリセットすべきかどうかの純粋な判定。
/// `blinkTimer`は`init`から常時走り続けており、SGR blinkセル/点滅カーソルが1つも
/// 無い画面が続いている間も位相は経過時間依存でトグルされ続ける。そのため
/// 「blink無し→blink有り」へ新規遷移する瞬間(前回の描画ではSGR blinkセルも点滅
/// カーソルも無かったが、今回はどちらか一方が有る)に単純にトグル済みの位相を
/// そのまま使うと、新しく現れたblinkが最初から「消灯」側で最大0.53秒不可視のまま
/// 表示されてしまう(fable/codexレビュー指摘、Android版`SshTerminalCanvas.kt`の
/// `LaunchedEffect(hasActiveBlink)`起動時リセットと対称)。`UIKit`/`Timer`に
/// 依存しない純粋関数として切り出してあるため`clampedFontScale`/`wheelEvents`と
/// 同様にユニットテストで直接検証できる。
func shouldResetBlinkPhase(
    newHasBlink: Bool, newCursorBlinks: Bool,
    previousHasBlink: Bool, previousCursorBlinks: Bool
) -> Bool {
    (newHasBlink || newCursorBlinks) && !(previousHasBlink || previousCursorBlinks)
}

/// Sixel(タスク#42)の`ImagePlacement.rgba`(RGBA8888、row-major)から`UIImage`を作って
/// idでキャッシュする。`ScreenUpdate.images`はTerminal(rust-core)側で寿命管理された
/// 「現在アクティブな画像の全リスト」がそのまま渡ってくる(rust-ssot: どの画像が
/// まだ生きているかの判断はRust側で完結している)ため、このクラスは判断ロジックを
/// 持たず「今回のリストに無いidの`UIImage`を捨て、まだキャッシュに無いidだけ新規
/// デコードする」宣言的な反映のみを行う(Android版`SixelBitmapCache`と対称)。
final class SixelBitmapCache {
    private var cache: [UInt64: UIImage] = [:]

    /// `placement`に対応する`UIImage`を返す(未キャッシュならデコードして格納する)。
    /// `draw(_:)`が`update.images`を毎回丸ごと走査し、そのidをそのまま渡す設計
    /// (呼び出し側が差分を判断する必要はない)。
    func image(for placement: ImagePlacement) -> UIImage? {
        if let cached = cache[placement.id] { return cached }
        guard let image = Self.decode(placement) else { return nil }
        cache[placement.id] = image
        return image
    }

    /// `liveIds`に無いエントリを捨てる。`ScreenUpdate.images`にもう出てこなくなった
    /// (＝Rust側で寿命が尽きた)画像のキャッシュを溜め込まないために呼ぶ。
    func prune(liveIds: Set<UInt64>) {
        cache = cache.filter { liveIds.contains($0.key) }
    }

    /// Rust側`sixel.rs`の`MAX_SIXEL_DIM`/`MAX_SIXEL_AREA`と同じ上限をここでも二重に
    /// 適用する。通常経路ではRust側で既に弾かれているはずだが、寸法とバッファ長が
    /// 矛盾する壊れた`ImagePlacement`が来た場合、`width * height * 4`のオーバーフロー
    /// トラップや巨大`CGImage`確保に直結させないための防御(codexレビュー指摘、
    /// Android版`SixelBitmapCache.isSane`と対称)。
    private static let maxDimension = 4096
    private static let maxArea = 4_000_000

    private static func decode(_ placement: ImagePlacement) -> UIImage? {
        let width = Int(placement.widthPx)
        let height = Int(placement.heightPx)
        guard width > 0, height > 0, width <= maxDimension, height <= maxDimension,
              width * height <= maxArea,
              placement.rgba.count == width * height * 4 else { return nil }
        guard let provider = CGDataProvider(data: placement.rgba as CFData) else { return nil }
        // 我々のデコーダ(`sixel.rs`)はalphaを常に0か255のいずれかでしか出力しない
        // (部分透過は生成しない)ため、premultiplied/straightどちらの解釈でも
        // 結果は同じになる——`premultipliedLast`を使う。
        guard let cgImage = CGImage(
            width: width,
            height: height,
            bitsPerComponent: 8,
            bitsPerPixel: 32,
            bytesPerRow: width * 4,
            space: CGColorSpaceCreateDeviceRGB(),
            bitmapInfo: CGBitmapInfo(rawValue: CGImageAlphaInfo.premultipliedLast.rawValue),
            provider: provider,
            decode: nil,
            shouldInterpolate: false,
            intent: .defaultIntent
        ) else { return nil }
        return UIImage(cgImage: cgImage)
    }
}

/// Phase 1D: ターミナル本画面の描画。Rust→Kotlin間で既に使われている
/// `ScreenUpdate`/`CellData`(ARGBパックの32bit色)を直接消費する
/// (Phase 1A-6の`TerminalFrameBatch`/`PackedRow`は診断用の並行表現であり、
/// 実際のレンダリング統合では使わないというPLAN.md記載の方針に従う)。
public final class TerminalScreenView: UIView, UIGestureRecognizerDelegate {
    private var latestUpdate: ScreenUpdate?
    private static let baseFontSize: CGFloat = 14
    private var font = UIFont.monospacedSystemFont(ofSize: baseFontSize, weight: .regular)
    private var boldFont = UIFont.monospacedSystemFont(ofSize: baseFontSize, weight: .bold)
    /// タスク#23: SGR 3(italic)/SGR 1+3(bold+italic)用のフォントバリアント。
    /// `monospacedSystemFont`にitalicウェイトは無いため、`font`/`boldFont`の
    /// `fontDescriptor`へ`.traitItalic`を合成して作る(等幅フォントファミリの
    /// 斜体グリフを使う。Android版`Typeface.create(base, Typeface.ITALIC)`と対称)。
    private var italicFont = UIFont.monospacedSystemFont(ofSize: baseFontSize, weight: .regular)
    private var boldItalicFont = UIFont.monospacedSystemFont(ofSize: baseFontSize, weight: .bold)
    private var cellSize: CGSize = .zero
    /// タスク#23: SGR 5(blink)属性が立っているセルの点滅位相。セッション状態の
    /// 一部ではなく純粋にUI表示上のアニメーションフェーズ(rust-ssot対象外——
    /// `CellData.blink`自体はRustが決定した値をそのまま見るだけで、「今どちらの
    /// 位相か」は表示にしか関わらない)。Android版も同種のタイマーをCanvas側で
    /// 持つ想定(タスク#22 Fableレビュー2次)。
    private var blinkPhaseVisible = true
    private var blinkTimer: Timer?
    /// 直近`draw(_:)`で実際に画面へ出したセル(`computeDisplayUpdate()`の結果、
    /// スクロールバック表示中はライブの`latestUpdate`とは異なる)にblink属性が
    /// 1つでもあったかどうか。blinkタイマーはこのキャッシュ値を見て`setNeedsDisplay()`
    /// を呼ぶかどうか決める——`latestUpdate`(常にライブ画面)を直接見てしまうと、
    /// スクロールバックを表示中(`scrollOffset > 0`)はライブ側にblinkが無ければ
    /// 点滅が止まり、逆にライブ側にだけblinkがあると無駄な再描画が走る
    /// (codexレビュー指摘)。`onScrollbackRequest`をタイマー刻みごとに呼ばずに済むよう
    /// `draw(_:)`実行時点の結果を保存するだけに留める。
    private var lastDisplayHasBlink = false
    /// タスク#34: 直近`draw(_:)`で実際にカーソルを点滅させる必要があったかどうか
    /// (`update.cursorVisible && update.cursorBlink`から導出、`cursorBlink`自体は
    /// DECSCUSR/`?12`でRustが決定した真値——rust-ssot:形状・点滅モードの判断は
    /// Rust側にあり、ここでは点滅の位相[`blinkPhaseVisible`]というUI表示専用状態を
    /// 管理するだけ)。`lastDisplayHasBlink`と同じ理由でキャッシュしておき、blink
    /// タイマーが無駄な再描画を避けられるようにする。
    private var lastDisplayCursorBlinks = false
    /// Sixel(タスク#42)の`ImagePlacement.rgba`から作った`UIImage`をidでキャッシュする
    /// (Android版`SixelBitmapCache`と対称)。
    private let sixelBitmapCache = SixelBitmapCache()

    /// Phase 1F-1(#48): 現在の選択範囲(行単位)。Android版`SelectionRange`と対称。
    /// 非nilの間`draw(_:)`でハイライトを描画する。
    public var selection: SelectionRange? {
        didSet { setNeedsDisplay() }
    }
    /// 選択範囲が変化する度に呼ばれる(SwiftUI側のフローティングツールバー表示に使う)。
    public var onSelectionChanged: ((SelectionRange?) -> Void)?

    /// タスク#67: 検索バーで現在選択中のマッチ位置(`SessionCore::search_scrollback`、
    /// #37が返した`ScrollbackSearchMatch`をそのまま保持するだけ——マッチ計算自体は
    /// 一切行わない、rust-ssot)。非nilかつ`scrollOffset`がその`row`と一致している間
    /// だけ`draw(_:)`でハイライトを描く(SwiftUI側`TerminalView`が検索バーの開閉・
    /// クエリ・「今何件目を見ているか」を保持し、ジャンプ時に`scrollOffset`を
    /// `match.row`へ合わせる設計、`TerminalView.swift`参照)。
    public var searchHighlight: ScrollbackSearchMatch? {
        didSet { setNeedsDisplay() }
    }

    /// Phase 1F-2(#49): フォントサイズの拡縮率(Android版`fontScale`、0.5〜3.0に
    /// クランプ、既定1.0)。SwiftUI側で`UserDefaults`(キー`"font_scale"`、Android版
    /// `SharedPreferences`の`"font_scale"`キーと対称)へ永続化する。
    public var fontScale: CGFloat = 1.0 {
        didSet {
            guard fontScale != oldValue else { return }
            updateFontMetrics()
            setNeedsDisplay()
        }
    }
    /// ピンチジェスチャで拡縮率が変化する度に呼ばれる(SwiftUI側での永続化に使う)。
    public var onFontScaleChanged: ((CGFloat) -> Void)?

    /// Phase 1F-4(#51): スクロールバックのスワイプで表示中のオフセット(0 = ライブ)。
    /// Android版`scrollOffset`と対称。SwiftUI側の「ライブへ戻る」ボタンからも
    /// (`selection`/`fontScale`と同様の双方向バインディングで)0を書き戻せる。
    public var scrollOffset: UInt32 = 0 {
        didSet {
            guard scrollOffset != oldValue else { return }
            if scrollOffset == 0 { panAccumY = 0 }
            onScrollOffsetChanged?(scrollOffset)
            setNeedsDisplay()
        }
    }
    /// タスク#79: `scrollOffset == 0`は従来「ライブ画面表示」を意味する唯一の条件として
    /// 使われてきたが、これだと検索結果の`row == 0`(scrollbackの最新履歴行)へジャンプする
    /// 際、`scrollOffset`を0にしてもライブ表示に横取りされて到達不能になっていた。
    /// 「ユーザーが明示的にscrollback表示へ入っているか」を`scrollOffset`の値そのものとは
    /// 独立したフラグとして持つことで、`scrollOffset == 0`のままscrollback最新行を表示
    /// できるようにする(`TerminalView.swift`の`showingScrollback`から`Binding`経由で
    /// 渡される、Android版`TerminalScreen.kt`の`showingScrollback`と対称)。
    public var showingScrollback: Bool = false {
        didSet {
            guard showingScrollback != oldValue else { return }
            setNeedsDisplay()
        }
    }
    /// スクロールバックの行を取得するクロージャ(Android版`actions.onScrollbackCells`相当)。
    public var onScrollbackRequest: ((_ offset: UInt32, _ rows: UInt32) -> [CellData])?
    /// スクロールバックの総行数を取得するクロージャ(Android版`uiState.scrollbackLen`相当)。
    public var onScrollbackLenRequest: (() -> UInt32)?
    /// スクロールオフセットが変化する度に呼ばれる(SwiftUI側の状態同期に使う)。
    public var onScrollOffsetChanged: ((UInt32) -> Void)?
    /// タスク#79: `handlePan`が手動でライブ方向へ戻し切った(`scrollOffset`が0に達した)
    /// 際、SwiftUI側の`showingScrollback`も解除するために呼ばれる(「ライブへ戻る」
    /// ボタンと同じ扱い)。
    public var onShowingScrollbackChanged: ((Bool) -> Void)?
    private var panAccumY: CGFloat = 0
    /// タスク#81: `wheelEvents(deltaY:carry:cellHeight:)`が返す「次回へ持ち越す端数」の
    /// 保持先。`panAccumY`とは別の蓄積系統(マウスモード有効時の間接scrollはローカルの
    /// `scrollOffset`を一切動かさずリモートへwheel up/downとして転送するだけのため)。
    private var wheelAccumY: CGFloat = 0

    /// タスク#52: OSC 8リンクをタップした時に呼ばれる(hit-testで有効なURLが見つかった
    /// 場合のみ)。SwiftUI側(`TerminalView`)がこれを受けて確認ダイアログを表示し、
    /// ユーザーが「開く」を選んだ場合のみ`UIApplication.open`を呼ぶ。URLは既に
    /// `isOpenableHyperlinkScheme`でhttp/httpsのみに絞り込み済み(Android版
    /// `pendingHyperlinkUrl`と対称)。
    public var onHyperlinkTapped: ((String) -> Void)?

    /// タスク#51: マウスレポーティング(`?1000`/`?1002`/`?1003`、SGR拡張`?1006`)が
    /// 有効な間、タッチをRust側でエンコードした生バイト列として送るためのフック
    /// (`onSendBytes`と同じ形——SwiftUI側が`controller.send(bytes)`に接続する)。
    /// エンコード自体は`terminalPointerEventBytes`(rust-core `terminal_pointer_event_bytes`、
    /// タスク#36/#51)がRust側で行い、このクラスは座標とジェスチャ種別を生のまま渡すだけ
    /// (rust-ssot: 「今どのマウスモードか」「このイベントを報告すべきか」の判断は
    /// Rust側の値をそのまま見るだけで、Swift側にミラー状態を作らない)。
    public var onPointerBytes: ((Data) -> Void)?

    /// タスク#51: 選択(`longPress`)・スクロールバックスワイプ(`pan`)・OSC 8タップ
    /// (`tap`)の各`UIGestureRecognizer`。マウスレポーティングが有効な間、これらに
    /// 単一指のタッチを渡さないようにする(`gestureRecognizer(_:shouldReceive:)`)ための
    /// 参照保持——`init`のローカル変数のままだと delegate 判定から参照できない。
    private var longPressGestureRecognizer: UILongPressGestureRecognizer?
    private var panGestureRecognizer: UIPanGestureRecognizer?
    private var tapGestureRecognizer: UITapGestureRecognizer?

    /// タスク#20: view bounds(実サイズ)とフォントのセルサイズから求めたcols/rowsが
    /// 変化する度に呼ばれる。Android版`TerminalScreen.kt`の
    /// `cols = (widthPx / cellDims.first).toInt().coerceAtLeast(10)` /
    /// `rows = (heightPx / cellDims.second).toInt().coerceAtLeast(5)` +
    /// `LaunchedEffect(cols, rows, connected)`と対称の計算(下限も同じ10/5)。
    /// 実際にRust側の`resize(cols:rows:)`へ転送するかどうかの判断・同一値の
    /// dedupeは呼び出し側(`TerminalScreenRepresentable`)/Rust側(`SessionCore::resize`、
    /// #62)に委ねる — ここでは「view sizeから求めたcols/rowsが変わった」という
    /// 生のジオメトリ計算結果を渡すだけ(rust-ssot: セッション状態の判断はしない)。
    public var onSizeChanged: ((UInt32, UInt32) -> Void)?
    private var lastReportedCols: UInt32?
    private var lastReportedRows: UInt32?
    private static let minCols: UInt32 = 10
    private static let minRows: UInt32 = 5

    public override init(frame: CGRect) {
        super.init(frame: frame)
        backgroundColor = .black
        contentMode = .redraw
        isOpaque = true
        updateFontMetrics()

        let longPress = UILongPressGestureRecognizer(target: self, action: #selector(handleLongPress(_:)))
        longPress.minimumPressDuration = 0.4
        longPress.delegate = self
        addGestureRecognizer(longPress)
        longPressGestureRecognizer = longPress

        let pinch = UIPinchGestureRecognizer(target: self, action: #selector(handlePinch(_:)))
        addGestureRecognizer(pinch)

        let pan = UIPanGestureRecognizer(target: self, action: #selector(handlePan(_:)))
        pan.maximumNumberOfTouches = 1
        pan.delegate = self
        // タスク#81: `allowedScrollTypesMask`の既定値(`.continuous`)はトラックパッドの
        // 連続スクロールのみを含み、外付けマウスのホイール(`.discrete`)を含まない
        // ——既定のままだと`isekai-ssh`側でマウスホイールを回してもこのgesture
        // recognizerが一切反応せず、`handlePan`のwheel経路にも到達できない。
        pan.allowedScrollTypesMask = [.discrete, .continuous]
        addGestureRecognizer(pan)
        panGestureRecognizer = pan

        // タスク#52: OSC 8リンクのタップhit-test用。素早いタップは
        // `UILongPressGestureRecognizer`の`minimumPressDuration`(0.4秒)未満で
        // 指が離れるため長押し認識には至らず、互いに競合しない。
        let tap = UITapGestureRecognizer(target: self, action: #selector(handleTap(_:)))
        tap.delegate = self
        addGestureRecognizer(tap)
        tapGestureRecognizer = tap

        startBlinkTimerIfNeeded()
    }

    public required init?(coder: NSCoder) {
        super.init(coder: coder)
        startBlinkTimerIfNeeded()
    }

    deinit {
        blinkTimer?.invalidate()
    }

    /// タスク#23/#34: 点滅位相を一定間隔(xterm既定に近い0.53秒)でトグルする。
    /// 現在の画面に実際にblink属性のセルも点滅カーソルも無ければ`setNeedsDisplay()`を
    /// 呼ばない(無駄な再描画でバッテリーを消費しない)。同じ`blinkPhaseVisible`位相を
    /// SGR 5(blink属性)と点滅カーソルの両方で共有する(xtermも同じ位相を共有する)。
    private func startBlinkTimerIfNeeded() {
        guard blinkTimer == nil else { return }
        blinkTimer = Timer.scheduledTimer(withTimeInterval: 0.53, repeats: true) { [weak self] _ in
            guard let self else { return }
            self.blinkPhaseVisible.toggle()
            if self.lastDisplayHasBlink || self.lastDisplayCursorBlinks {
                self.setNeedsDisplay()
            }
        }
    }

    /// タスク#86 codexレビュー2次指摘: `blinkPhaseVisible`をtrueへ戻すだけでは、
    /// `blinkTimer`自体は`init`以来の古いスケジュールのまま動き続けている。新規blink
    /// 出現の直後にたまたま次のtickが目前だと、「一瞬だけ見えてすぐ消灯」という
    /// 短いflickerになりかねない(Android版`LaunchedEffect(hasActiveBlink)`は
    /// コルーチン自体を再起動し`delay(530)`から数え直すため、新規blinkは必ず満額
    /// 0.53秒の可視区間を得る——iOS側もタイマーのスケジュールを起点からやり直すことで
    /// 同じ保証を揃える)。
    private func restartBlinkTimer() {
        blinkTimer?.invalidate()
        blinkTimer = nil
        startBlinkTimerIfNeeded()
    }

    /// 最新の画面状態を反映する。`MainActor`から呼ぶこと。
    public func apply(_ update: ScreenUpdate) {
        latestUpdate = update
        setNeedsDisplay()
    }

    private func updateFontMetrics() {
        let size = Self.baseFontSize * fontScale
        font = UIFont.monospacedSystemFont(ofSize: size, weight: .regular)
        boldFont = UIFont.monospacedSystemFont(ofSize: size, weight: .bold)
        italicFont = Self.italicVariant(of: font)
        boldItalicFont = Self.italicVariant(of: boldFont)
        let measured = ("M" as NSString).size(withAttributes: [.font: font])
        cellSize = CGSize(width: measured.width, height: font.lineHeight)
        // タスク#20: ピンチズームでcellSizeが変わればcols/rowsも変わりうる
        // (Android版`cellDims`が`fontScale`込みの`remember`キーになっているのと対称)。
        reportSizeIfNeeded()
    }

    public override func layoutSubviews() {
        super.layoutSubviews()
        // タスク#20: 画面回転・SplitView/Slide Overサイズ変更等でboundsが変わった度に
        // cols/rowsを再計算する(Android版`BoxWithConstraints`が`maxWidth`/`maxHeight`の
        // 変化を検知するのと対称)。
        //
        // 既知の制約(タスク#19のAndroid版`computeResizeTargetColsRows`の`imeBottomPx`
        // 補正に相当する処理は未実装): ソフトキーボード表示中にSwiftUI側の既定の
        // キーボード回避レイアウトでこのviewのboundsが実際に縮む場合、その分も
        // 通常のリサイズとしてそのまま転送してしまう(IME開閉自体をtty実サイズ変更の
        // 理由にしたくないというAndroid版#19の方針とは未整合)。iOS側でのSwiftUI-UIKit
        // 間のキーボード回避挙動はOSバージョンや実機検証なしには正確に補正できないと
        // 判断し、このタスク(#20)の対象範囲(動的resize自体の実装)からは切り出した。
        // Rust側`SessionCore::resize`の同一値dedupe(#62)により永続的な状態不整合には
        // ならない(キーボードを閉じれば実サイズへ戻る)。
        reportSizeIfNeeded()
    }

    /// タスク#20: `TerminalSessionController.connect()`は実際のview sizeが判明する前に
    /// 既定の80x24でセッションを開始する。接続確立(再接続含む)直後に、既知のview実
    /// サイズへ確実に一度合わせ直すためのフック(Android版`LaunchedEffect(cols, rows,
    /// connected)`が`connected`もキーに含めることで、cols/rowsの値自体は変わらなくても
    /// 「接続状態が変わった」場合に確実に再発火するのと同じ理由)。
    /// `TerminalScreenRepresentable.updateUIView`から接続状態の遷移を検知して呼ばれる。
    public func resendSizeOnConnectionEstablished() {
        lastReportedCols = nil
        lastReportedRows = nil
        reportSizeIfNeeded()
    }

    private func reportSizeIfNeeded() {
        guard cellSize.width > 0, cellSize.height > 0, bounds.width > 0, bounds.height > 0 else { return }
        let cols = max(UInt32(bounds.width / cellSize.width), Self.minCols)
        let rows = max(UInt32(bounds.height / cellSize.height), Self.minRows)
        guard cols != lastReportedCols || rows != lastReportedRows else { return }
        lastReportedCols = cols
        lastReportedRows = rows
        onSizeChanged?(cols, rows)
    }

    /// ピンチズームでのフォントサイズ調整(Android版`TerminalScreen.kt`の
    /// `event.calculateZoom()`+`fontScale.coerceIn(0.5f, 3.0f)`と対称)。
    @objc private func handlePinch(_ recognizer: UIPinchGestureRecognizer) {
        guard recognizer.state == .changed else { return }
        let newScale = clampedFontScale(current: fontScale, zoomDelta: recognizer.scale)
        recognizer.scale = 1.0
        guard newScale != fontScale else { return }
        fontScale = newScale
        onFontScaleChanged?(newScale)
    }

    /// スクロールバックのスワイプ(Android版`TerminalScreen.kt`の`panAccumY`+
    /// `event.calculatePan()`ループと対称)。縦方向のドラッグ量を蓄積し、セル1行分
    /// 溜まる度に`scrollOffset`を1ずつ増減する。長押し(選択)が既に認識されている間は
    /// UIKitの既定動作(同一ビュー上の複数ジェスチャの同時認識は既定でOFF)により
    /// このpanは発火しない。
    ///
    /// タスク#81: `recognizer.numberOfTouches == 0`は、この`.changed`がスクリーン上の
    /// 指のドラッグではなくトラックパッド/マウスホイールの間接scrollによって発火した
    /// ことを示す(画面に実際に触れている指が無いのに`UIPanGestureRecognizer`が反応
    /// するのは間接入力の場合のみ)。マウスレポーティングが有効(`isPointerReportingActive`)
    /// な間は、この間接scrollをローカルの`scrollOffset`ではなくxterm wheel up/down
    /// (button 64/65)としてリモートへ転送する(Android版`PointerEventType.Scroll`経路
    /// [`TerminalScreen.kt`]と対称)。指によるタッチスワイプ(`numberOfTouches > 0`)は
    /// マウスモード中は`gestureRecognizer(_:shouldReceive:)`でそもそもこのrecognizer自体に
    /// 渡らない(既存挙動のまま)。
    ///
    /// codexレビュー指摘: 一連のジェスチャ(`.ended`/`.cancelled`/`.failed`)が終わった時点で
    /// `wheelAccumY`の端数を持ち越すと、無関係な次のスクロール操作(あるいはマウスモードが
    /// 一旦OFFになって再度ONになった後)で「本来はまだセル1行分溜まっていないのに」早すぎる
    /// `WheelUp`/`WheelDown`が出てしまう。ジェスチャの区切りごとにリセットする。
    @objc private func handlePan(_ recognizer: UIPanGestureRecognizer) {
        if recognizer.state == .ended || recognizer.state == .cancelled || recognizer.state == .failed {
            wheelAccumY = 0
            return
        }
        guard recognizer.state == .changed, cellSize.height > 0 else { return }
        let translation = recognizer.translation(in: self)
        recognizer.setTranslation(.zero, in: self)

        if recognizer.numberOfTouches == 0, isPointerReportingActive, let update = latestUpdate {
            let (buttons, carry) = wheelEvents(deltaY: translation.y, carry: wheelAccumY, cellHeight: cellSize.height)
            wheelAccumY = carry
            guard !buttons.isEmpty else { return }
            let point = recognizer.location(in: self)
            for button in buttons {
                sendPointerEvent(at: point, update: update, kind: .press, button: button)
            }
            return
        }

        panAccumY += translation.y

        let scrollbackLen = onScrollbackLenRequest?() ?? 0
        let cellHeight = cellSize.height
        while panAccumY < -cellHeight {
            if scrollOffset < scrollbackLen { scrollOffset += 1 }
            panAccumY += cellHeight
        }
        while panAccumY > cellHeight {
            if scrollOffset > 0 { scrollOffset -= 1 }
            // タスク#79: 手動でライブ方向へパンし0まで戻したら、検索ジャンプ由来の
            // `showingScrollback`も解除する(「ライブへ戻る」ボタンと同じ扱い)。
            if scrollOffset == 0, showingScrollback {
                showingScrollback = false
                onShowingScrollbackChanged?(false)
            }
            panAccumY -= cellHeight
        }
    }

    /// `scrollOffset`が0かつ`showingScrollback`が偽ならライブの`latestUpdate`をそのまま、
    /// それ以外は`onScrollbackRequest`でスクロールバックの行を取得してカーソルを画面外に
    /// 隠した`ScreenUpdate`を合成する(Android版`displayUpdate`の
    /// `remember(scrollOffset, showingScrollback, ...)`と同じ役割。タスク#79)。
    private func computeDisplayUpdate() -> ScreenUpdate? {
        guard let update = latestUpdate else { return nil }
        guard scrollOffset > 0 || showingScrollback else { return update }
        let cells = onScrollbackRequest?(scrollOffset, update.rows) ?? []
        return synthesizeDisplayUpdate(
            live: update, scrollOffset: scrollOffset, scrollbackCells: cells, showingScrollback: showingScrollback
        )
    }

    /// 長押し+ドラッグでの行単位テキスト選択(Android版`TerminalScreen.kt`の
    /// `awaitLongPressOrCancellation`+ドラッグループと対称)。`UILongPressGestureRecognizer`は
    /// `.began`後の移動でも認識状態を維持し続けて`.changed`を報告し続けるため、
    /// 別途pan gestureを組み合わせる必要はない。
    @objc private func handleLongPress(_ recognizer: UILongPressGestureRecognizer) {
        guard let update = computeDisplayUpdate() else { return }
        let cols = Int(update.cols)
        let rows = Int(update.rows)
        let point = recognizer.location(in: self)
        let cell = offsetToCellPos(x: Double(point.x), y: Double(point.y), cellWidth: Double(cellSize.width), cellHeight: Double(cellSize.height), cols: cols, rows: rows)

        switch recognizer.state {
        case .began:
            let newSelection = SelectionRange(anchor: cell, head: cell)
            selection = newSelection
            onSelectionChanged?(newSelection)
        case .changed:
            guard var current = selection else { return }
            current.head = cell
            selection = current
            onSelectionChanged?(current)
        default:
            break
        }
    }

    /// タスク#52: OSC 8リンクのタップhit-test。hit-test自体は表示中のセル配列を
    /// 読むだけのUI表示に閉じた判断であり、rust-ssot原則の対象外(`linkId`/
    /// `linkTable`は既にRust側がintern済みで公開している、Android版
    /// `TerminalScreen.kt`の`linkUrlAtCell`呼び出しと対称)。リンクが無い、または
    /// スキームがhttp/https以外の場合は何もしない(`isOpenableHyperlinkScheme`で
    /// `intent://`等を無条件で開かないようにする、タスク#52 Fableレビュー2次)。
    @objc private func handleTap(_ recognizer: UITapGestureRecognizer) {
        guard let update = computeDisplayUpdate() else { return }
        let cols = Int(update.cols)
        let rows = Int(update.rows)
        let point = recognizer.location(in: self)
        let cell = offsetToCellPos(x: Double(point.x), y: Double(point.y), cellWidth: Double(cellSize.width), cellHeight: Double(cellSize.height), cols: cols, rows: rows)
        guard let url = linkURL(at: update, row: cell.row, col: cell.col), isOpenableHyperlinkScheme(url) else { return }
        onHyperlinkTapped?(url)
    }

    // ── マウスレポーティング(タスク#36/#51) ──────────────────────────

    /// 現在マウスイベントとして追跡中のタッチ。`touchesBegan`で単一指のタッチが
    /// 始まった時にpressを送って設定し、そのタッチが離れる/取り消される、または
    /// 2本目の指が触れて複数指になった時点でreleaseを送って`nil`に戻す
    /// (codexレビュー指摘: 2本目の指が触れた後の`moved`/`ended`を単純に無視すると、
    /// 直前に送ったpressに対応するreleaseが送られず、リモート側でボタンが
    /// 押されっぱなしに見えるバグになっていた)。
    private weak var activeMouseTouch: UITouch?

    /// マウスレポーティング(`?1000`/`?1002`/`?1003`)が有効かつスクロールバック表示中
    /// (`scrollOffset > 0`)でない間、選択(`longPress`)・スクロールバックスワイプ
    /// (`pan`)・OSC 8タップ(`tap`)へタッチを渡さないようにする。これらは全て単一指の
    /// ジェスチャで、有効な間は代わりに`touchesBegan`/`touchesMoved`/`touchesEnded`
    /// (下記)がマウスのpress/drag/releaseとして同じタッチを処理する。ピンチ
    /// (2本指ズーム)はマウスレポートと衝突しないため対象外のまま残す。
    ///
    /// `latestUpdate?.mouseReportingMode`(rust-core `Terminal`が保持する真値、
    /// `ScreenUpdate`経由でそのまま読むだけ)を毎回見て判断するだけで、「今マウス
    /// モードか」をこのクラス側の別状態としてミラーしない(rust-ssot、タスク#51)。
    public func gestureRecognizer(_ gestureRecognizer: UIGestureRecognizer, shouldReceive touch: UITouch) -> Bool {
        guard gestureRecognizer === longPressGestureRecognizer
            || gestureRecognizer === panGestureRecognizer
            || gestureRecognizer === tapGestureRecognizer else { return true }
        return !isPointerReportingActive
    }

    /// マウスレポーティングが実際に有効か。モードが`.off`でないことに加え、
    /// スクロールバック表示中(`scrollOffset > 0`、またはタスク#79の`showingScrollback`)は
    /// 対象外とする(codexレビュー指摘: `draw(_:)`はスクロールバックの合成表示を見せている
    /// 一方でライブ側のモードに従ってポインタイベントを送ると、ユーザーは過去ログを
    /// 見ているのにライブセッションへclick/dragが飛んでしまい、表示対象と入力対象が
    /// 食い違う)。
    private var isPointerReportingActive: Bool {
        guard scrollOffset == 0, !showingScrollback, let update = latestUpdate else { return false }
        return update.mouseReportingMode != .off
    }

    /// `touch`の現在位置をLeftボタンのイベントとして`sendPointerEvent(at:update:kind:button:)`
    /// へ渡す(iOS側のタッチにはボタン無しの単純なホバー移動の概念が無いため、タッチしている
    /// 間は常にLeftボタンを押しているとみなす)。
    private func sendMouseEvent(for touch: UITouch, update: ScreenUpdate, kind: MouseEventKind) {
        sendPointerEvent(at: touch.location(in: self), update: update, kind: kind, button: .left)
    }

    /// view座標(`point`)を`terminalPointerEventBytes`(rust-core、タスク#36/#51)へ
    /// 渡して結果を`onPointerBytes`で送出する共通処理。タッチ由来のLeftボタンイベント
    /// (`sendMouseEvent`)とトラックパッド/マウスホイール由来のwheel up/downイベント
    /// (タスク#81、`handlePan`)の両方から使う(座標→セル変換+送出の重複を避ける、
    /// Android版`sendPointerEventAt`と対称)。
    private func sendPointerEvent(at point: CGPoint, update: ScreenUpdate, kind: MouseEventKind, button: MouseButton?) {
        let cell = offsetToCellPos(x: Double(point.x), y: Double(point.y), cellWidth: Double(cellSize.width), cellHeight: Double(cellSize.height), cols: Int(update.cols), rows: Int(update.rows))
        guard let bytes = terminalPointerEventBytes(
            kind: kind,
            button: button,
            row: UInt32(cell.row),
            col: UInt32(cell.col),
            modifiers: TerminalKeyModifiers(shift: false, alt: false, ctrl: false, meta: false),
            cols: update.cols,
            rows: update.rows,
            mouseReportingMode: update.mouseReportingMode,
            sgrMouseMode: update.sgrMouseMode
        ) else { return }
        onPointerBytes?(bytes)
    }

    /// iOS側のタッチにはボタン無しの単純なホバー移動の概念が無いため、タッチしている
    /// 間は常にLeftボタンを押しているとみなす(`button`は常に`.left`)。
    public override func touchesBegan(_ touches: Set<UITouch>, with event: UIEvent?) {
        super.touchesBegan(touches, with: event)
        guard isPointerReportingActive, let update = latestUpdate else { return }
        if let active = activeMouseTouch {
            // 追跡中に2本目以降の指が触れた: これ以上単一指のドラッグとしては扱えない
            // ため、既に送ったpressに対応するreleaseを送って打ち切る(以降はpinch等の
            // 通常の複数指ジェスチャに譲り、この一連のタッチは無視する)。
            sendMouseEvent(for: active, update: update, kind: .release)
            activeMouseTouch = nil
            return
        }
        guard (event?.allTouches?.count ?? touches.count) == 1, let touch = touches.first else { return }
        activeMouseTouch = touch
        sendMouseEvent(for: touch, update: update, kind: .press)
    }

    public override func touchesMoved(_ touches: Set<UITouch>, with event: UIEvent?) {
        super.touchesMoved(touches, with: event)
        guard let update = latestUpdate, let active = activeMouseTouch, touches.contains(active) else { return }
        sendMouseEvent(for: active, update: update, kind: .motion)
    }

    public override func touchesEnded(_ touches: Set<UITouch>, with event: UIEvent?) {
        super.touchesEnded(touches, with: event)
        guard let update = latestUpdate, let active = activeMouseTouch, touches.contains(active) else { return }
        sendMouseEvent(for: active, update: update, kind: .release)
        activeMouseTouch = nil
    }

    public override func touchesCancelled(_ touches: Set<UITouch>, with event: UIEvent?) {
        super.touchesCancelled(touches, with: event)
        guard let update = latestUpdate, let active = activeMouseTouch, touches.contains(active) else { return }
        sendMouseEvent(for: active, update: update, kind: .release)
        activeMouseTouch = nil
    }

    public override func draw(_ rect: CGRect) {
        guard let update = computeDisplayUpdate() else { return }
        let cols = Int(update.cols)
        let rows = Int(update.rows)
        guard cols > 0, rows > 0, update.cells.count == cols * rows else { return }
        let newHasBlink = update.cells.contains(where: { $0.blink })
        let cursorInBounds = Int(update.cursorRow) < rows && Int(update.cursorCol) < cols
        let newCursorBlinks = update.cursorVisible && update.cursorBlink && cursorInBounds
        if shouldResetBlinkPhase(
            newHasBlink: newHasBlink, newCursorBlinks: newCursorBlinks,
            previousHasBlink: lastDisplayHasBlink, previousCursorBlinks: lastDisplayCursorBlinks
        ) {
            blinkPhaseVisible = true
            restartBlinkTimer()
        }
        lastDisplayHasBlink = newHasBlink

        let cellWidth = cellSize.width
        let cellHeight = cellSize.height

        for row in 0..<rows {
            for col in 0..<cols {
                let cell = update.cells[row * cols + col]
                let x = CGFloat(col) * cellWidth
                let y = CGFloat(row) * cellHeight
                let cellRect = CGRect(x: x, y: y, width: cellWidth, height: cellHeight)

                // reverse(SGR 7)は`terminal.rs`のSGRパース時点で`fg`/`bg`へ実効色として
                // 解決済み(このコードベースの一貫した方針、#21 Fableレビュー2次)なので、
                // ここでは`cell.fg`/`cell.bg`をそのまま使うだけでよく、reverse自体を
                // 見て入れ替える必要は無い。
                let bg = Self.colorFromPackedArgb(cell.bg)
                bg.setFill()
                UIRectFill(cellRect)

                // 空白文字自体は本来drawするグリフが無いが、underline/strikethrough
                // (SGR 4/9)が立っている空白セルは装飾線だけ描く必要があるため、
                // 早期スキップの対象から除外する(Android版`SshTerminalCanvas.kt`の
                // `hasLineDecoration`と対称。codexレビュー・fableレビュー両方が
                // 独立に指摘、タスク#71)。
                let hasLineDecoration = cell.underline || cell.strikethrough
                guard !cell.ch.isEmpty, cell.ch != " " || hasLineDecoration else { continue }
                // invisible(SGR 8)は背景だけ塗ってグリフを描かない。blink(SGR 5)は
                // 点滅位相が「消灯」側の間だけ同様にグリフを省く(背景・選択範囲・
                // カーソルの重なりは通常通り)。
                guard !cell.invisible, !(cell.blink && !blinkPhaseVisible) else { continue }

                var fg = Self.colorFromPackedArgb(cell.fg)
                if cell.dim {
                    // dim(SGR 2)は色そのものを再計算するのではなく、不透明度を下げて
                    // 背景と混ぜることで暗く見せる(Rust側は色をパース時にARGB解決する
                    // 方針のため、dimによる減光は表示側の責務)。
                    fg = fg.withAlphaComponent(0.6)
                }

                let resolvedFont: UIFont
                switch (cell.bold, cell.italic) {
                case (true, true): resolvedFont = boldItalicFont
                case (true, false): resolvedFont = boldFont
                case (false, true): resolvedFont = italicFont
                case (false, false): resolvedFont = font
                }

                var attrs: [NSAttributedString.Key: Any] = [
                    .font: resolvedFont,
                    .foregroundColor: fg,
                ]
                if cell.underline {
                    attrs[.underlineStyle] = NSUnderlineStyle.single.rawValue
                    attrs[.underlineColor] = fg
                }
                if cell.strikethrough {
                    attrs[.strikethroughStyle] = NSUnderlineStyle.single.rawValue
                    attrs[.strikethroughColor] = fg
                }
                (cell.ch as NSString).draw(at: CGPoint(x: x, y: y), withAttributes: attrs)
            }
        }

        // Sixel画像(タスク#42)。テキストグリッドの上・カーソル/選択ハイライトの下に
        // 重ねる(Android版`SshTerminalCanvas.kt`と同じ描画順)。配置(row/col/
        // rows_span/cols_span)の判断は一切ここでは行わず、Rust側が決めた矩形へ
        // `rgba`を引き伸ばして描くだけ(rust-ssot)。ビットマップ自体はidをキーに
        // キャッシュし(Android版`SixelBitmapCache`と対称)、同じ画像を毎フレーム
        // デコードし直さない。
        for placement in update.images {
            guard let image = sixelBitmapCache.image(for: placement) else { continue }
            let dstRect = CGRect(
                x: CGFloat(placement.col) * cellWidth,
                y: CGFloat(placement.row) * cellHeight,
                width: CGFloat(placement.colsSpan) * cellWidth,
                height: CGFloat(placement.rowsSpan) * cellHeight
            )
            image.draw(in: dstRect)
        }
        sixelBitmapCache.prune(liveIds: Set(update.images.map(\.id)))

        // 選択範囲のハイライト(行単位)。Android版`SshTerminalCanvas.kt`はセル背景の
        // 前(下)に半透明色を敷くが、iOS版は各セルの背景を無条件に不透明で塗るため
        // (上のループ参照)、ここでは代わりにセル描画の後にオーバーレイとして重ねる。
        if let selection {
            let startRow = min(max(selection.startRow, 0), rows - 1)
            let endRow = min(max(selection.endRow, 0), rows - 1)
            if startRow <= endRow {
                UIColor.white.withAlphaComponent(120.0 / 255.0).setFill()
                for row in startRow...endRow {
                    let y = CGFloat(row) * cellHeight
                    UIRectFill(CGRect(x: 0, y: y, width: CGFloat(cols) * cellWidth, height: cellHeight))
                }
            }
        }

        // タスク#67: 検索バーの現在マッチのハイライト。`ScrollbackSearchMatch.row`は
        // `scrollback_cells`と同じ規約("offset"がそのまま`row`)なので、`scrollOffset`が
        // その値と一致する場合に限り、その行は`computeDisplayUpdate()`が返す表示グリッド
        // の最終行(row = rows - 1)に現れる(`scrollback_cells`の`sb_idx = offset +
        // (rows-1-r)`で`r = rows-1`のとき`sb_idx == offset`になることから導ける、
        // `session.rs`の`scrollback_cells_orders_oldest_to_newest_top_to_bottom`テスト
        // 参照)。`scrollOffset`がまだ追従していない(ジャンプ直後の再描画が来る前)場合は
        // 描画しない。タスク#79: `row == 0`(scrollback最新行)は`scrollOffset == 0`が
        // 「ライブ画面表示」を兼ねる既存規約と衝突するため、`showingScrollback`が真の間
        // (=実際にscrollback最新行を表示中)だけ許可する——ライブ画面表示中
        // (`scrollOffset == 0 && !showingScrollback`)にrow=0のマッチを誤ってハイライト
        // しない。マッチの位置計算は一切ここでは行わず、Rust側`search_scrollback`が
        // 返した座標をそのまま描くだけ(rust-ssot)。
        if let searchHighlight, scrollOffset == searchHighlight.row, searchHighlight.row != 0 || showingScrollback {
            let highlightRow = rows - 1
            let startCol = min(Int(searchHighlight.col), cols)
            let endCol = min(startCol + Int(searchHighlight.len), cols)
            if startCol < endCol {
                UIColor.systemYellow.withAlphaComponent(0.55).setFill()
                let y = CGFloat(highlightRow) * cellHeight
                let x = CGFloat(startCol) * cellWidth
                let width = CGFloat(endCol - startCol) * cellWidth
                UIRectFill(CGRect(x: x, y: y, width: width, height: cellHeight))
            }
        }

        // DECTCEM(CSI ?25l/h)でカーソルが非表示状態のときはRust側が`cursorVisible = false`を
        // 立てるので、描画自体をスキップする(rust-ssot: 可視判定はRust側で行い、Swift側は
        // フラグをそのまま反映するだけ。Android版`SshTerminalCanvas.kt`の`update.cursorVisible`
        // ガードと対称)。タスク#34: DECSCUSR(`CSI Ps SP q`)が選択した形状は
        // `update.cursorShape`(Rust側`Terminal`が真値を保持、rust-ssot)からそのまま読み、
        // block/underline/barを描き分ける。点滅そのもの(`blinkPhaseVisible`という位相)は
        // UIローカル状態(タスク#23のSGR blinkと同じ`Timer`を共有)だが、「点滅させるべきか
        // どうか」は`update.cursorBlink`(DECSCUSRの偶数/奇数パラメータ、DECSET `?12`の
        // どちらもRust側`Terminal`が解決済み)をそのまま見るだけで、Swift側では判断しない。
        // タスク#34 codexレビュー指摘: スクロールバック表示中は`synthesizeDisplayUpdate`が
        // `cursorRow = update.rows`(画面外)にしてカーソルを隠すため、その場合は
        // `cursorInBounds`がfalseになり、実際には描画されないカーソルの点滅のために
        // blinkタイマーが毎tick`setNeedsDisplay()`する無駄を避ける(`lastDisplayHasBlink`が
        // 「実際に画面へ出した表示」を基準にしているのと同じ方針)。
        lastDisplayCursorBlinks = newCursorBlinks
        if update.cursorVisible, cursorInBounds,
           !(update.cursorBlink && !blinkPhaseVisible) {
            let x = CGFloat(update.cursorCol) * cellWidth
            let y = CGFloat(update.cursorRow) * cellHeight
            UIColor.white.withAlphaComponent(0.5).setFill()
            let cursorRect: CGRect
            switch update.cursorShape {
            case .block:
                cursorRect = CGRect(x: x, y: y, width: cellWidth, height: cellHeight)
            case .underline:
                let thickness = max(2.0, cellHeight * 0.12)
                cursorRect = CGRect(x: x, y: y + cellHeight - thickness, width: cellWidth, height: thickness)
            case .bar:
                let thickness = max(2.0, cellWidth * 0.15)
                cursorRect = CGRect(x: x, y: y, width: thickness, height: cellHeight)
            }
            UIRectFill(cursorRect)
        }
    }

    /// `baseFont`のフォントファミリの斜体バリアントを返す(見つからなければ
    /// `baseFont`自体を返す——太字斜体が用意されていないシステムフォントでも
    /// クラッシュせず、フォールバックとして非斜体のまま描画される)。
    private static func italicVariant(of baseFont: UIFont) -> UIFont {
        let traits = baseFont.fontDescriptor.symbolicTraits.union(.traitItalic)
        guard let descriptor = baseFont.fontDescriptor.withSymbolicTraits(traits) else { return baseFont }
        return UIFont(descriptor: descriptor, size: baseFont.pointSize)
    }

    /// Android版`CellData.fg`/`bg`と同じARGBパック形式(0xAARRGGBB)として解釈する
    /// (`ui/SshTerminalCanvas.kt`が`cell.bg.toInt()`をAndroidの`Color` intとして
    /// そのまま使っているのと対称)。
    private static func colorFromPackedArgb(_ value: UInt32) -> UIColor {
        let a = CGFloat((value >> 24) & 0xFF) / 255.0
        let r = CGFloat((value >> 16) & 0xFF) / 255.0
        let g = CGFloat((value >> 8) & 0xFF) / 255.0
        let b = CGFloat(value & 0xFF) / 255.0
        return UIColor(red: r, green: g, blue: b, alpha: a == 0 ? 1.0 : a)
    }
}
