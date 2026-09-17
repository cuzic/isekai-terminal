# ADR: Android版STUN P2Pの真の再ランデブー時にresume連続性を保つ

- **Status**: Draft(2026-09-16起草。Windows `isekai-ssh` との接続安定化
  ギャップ分析セッションから派生。`ADR_STUN_REESTABLISH_CONTINUITY.md`
  §5「Androidとの関係(対象外)」の記述を直接の出発点とする。実装着手前に
  レビュー要——本ADRは`opus-adversarial-consult`の対象には含めていない
  (ユーザー指定)が、頻度計測(§5)を済ませてから改めて着手判断すべき)
- **対象**(見込み、要精査): `rust-core/isekai-pipe/src/engine/mod.rs`
  (serve側、`--punch-peer`のワンショット制約解消・ライブ制御コマンド追加)、
  `rust-core/src/isekai_stun_p2p_transport.rs`(Android側の再punch駆動)
- **入力**: `ADR_ANDROID_RECONNECT_TIMEOUT.md`と同一のセッション
- **拘束される既存ルール**: `.claude/rules/always-connects.md`。
  `ADR_STUN_REESTABLISH_CONTINUITY.md`§5(本ADRの前提となる分析)

---

## 1. 背景

`ADR_STUN_REESTABLISH_CONTINUITY.md`(Approved rev4、Windows/isekai-ssh向け、
未実装)は、STUN P2Pセッションで**クライアント側アドレスのみ**が変わる
ケースを`isekai-pipe connect`経由のcross-family(relay)フォールバックで
救う設計だが、同ADR §5は次のように明記してAndroidを対象外としている:

> Androidは`isekai-pipe connect`プロセスを一切起動せず`isekai-transport`
> を直接in-processで使う構造のため、本ADRの変更は構造上Androidに何の
> 影響も与えない(「方針として除外した」のではなく「そもそも到達しない」)。
> Androidで同種の改善をするなら、serve側の再punch制御コマンド
> (`isekai-pipe ctl`相当の追加)が前提タスクとして別途必要になる——
> これは本ADRの範囲外の独立した設計課題として切り出す。

Android(`isekai_stun_p2p_transport.rs`)は既に`resume_client::ReattachableStream`
ベースのreattachを持ち、同一トランスポート内でのbare redial(サーバー側
アドレスが安定していれば再STUN・再punch無しで張り直す)には対応している
(`ADR_ANDROID_MIDSESSION_RESUME_BUDGET.md`が扱う予算の範囲内)。しかし
**クライアント・サーバー双方のアドレスが同時に変わる真の再ランデブー**
には対応できない。理由はWindows版と異なる:

- `--punch-peer`は`isekai-pipe serve`起動時のワンショット引数
  (`isekai-pipe/src/engine/mod.rs:646-670`)であり、稼働中の`serve`に
  「新しいクライアントアドレスへ再punchせよ」と伝えるライブ制御コマンドが
  存在しない。
- 新しい`serve`を立ち上げ直すとsession_secret・証明書が変わり、旧セッション
  には原理的に到達できなくなる(=resume連続性が失われる)。

## 2. 問題

スマホは「クライアント側アドレスが変わる」機会がノートPC以上に多い
(Wi-Fi⇔セルラー切替、移動によるセルタワー切替)。サーバー側
(自宅ルーター等)も再起動やISPの動的IP再割当で稀に変わりうる。
両方が同時に変わった場合、現状のAndroidはbare redialが失敗し、
`ADR_ANDROID_MIDSESSION_RESUME_BUDGET.md`/`ADR_ANDROID_RECONNECT_TIMEOUT.md`
の予算を使い切った末に、**新セッション扱い(scrollback/未確認バイトの
連続性喪失)として再接続する**ことになる。接続そのものは
`always-connects.md`の原則通り自動復旧する見込みだが、連続性は失われる。

## 3. 実装方針(2段階)

**段階A(前提、`isekai-pipe`側)**: `isekai-pipe serve`に、稼働中プロセスへ
「新しいclient観測アドレスへ再STUN+再punchせよ」と伝えるライブ制御
コマンドを追加する。既存の`isekai-pipe ctl`(tmux ctl-socket forward等で
既に使われている制御チャネル、`CLAUDE.md`記載)のパターンを再利用できないか
検討する。

**段階B(Android側)**: `isekai_stun_p2p_transport.rs`に、bare redial
(既存reattach)が一定回数/一定時間失敗した場合、自分自身のSTUN観測アドレスを
取り直し、段階Aの制御コマンド経由で`serve`に伝えて再punchを試みる経路を
追加する。

## 4. 非目標

- relay系(`isekai_link_relay_transport.rs`、未実装)を経由した
  cross-family fallbackは本ADRでは扱わない(`ADR_ANDROID_STUN_P2P_AUTO_FALLBACK.md`
  の領域)。
- Windows版(`ADR_STUN_REESTABLISH_CONTINUITY.md`)との実装共有は目指さない
  ——`isekai-pipe connect`サブプロセス経由(Windows)と、in-processの
  `isekai-transport`直接利用(Android)はアーキテクチャが異なる。

## 5. Open Questions / 着手前提

- **最優先**: 「クライアント・サーバー双方が同時にアドレスを変える」
  ケースの実発生頻度は未計測。`ADR_STUN_REESTABLISH_CONTINUITY.md` §3.2
  タスク5が導入する計装(`"continuity-lost"`ケースの検知)と同様の
  アプローチを、Android側にもまず入れて実際の頻度を計測してから
  実装着手を判断すべき(5つのADRの中で最も実装コストが高く、費用対効果が
  未知数なため)。
- 段階Aの制御コマンドのセキュリティ設計: 「誰が"再punchせよ"と言えるのか」
  ——session_secret相当の認証をどう挟むか、なりすまし要求で意図しない
  アドレスへ穴あけさせられないか。
- 本ADRの段階Aは、`ADR_STUN_REESTABLISH_CONTINUITY.md`側の実装状況
  (`--punch-peer`ワンショット制約の解消を先方が別の理由で先に成し遂げる
  可能性)を実装着手前に再確認し、重複実装を避けること。

## 6. 参照実装

(実装後に追記)
