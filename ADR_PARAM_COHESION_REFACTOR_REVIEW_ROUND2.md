# Review Round 2: `ADR_PARAM_COHESION_REFACTOR.md`(opus 2体、独立並行レビュー)

round 1の指摘を反映したADR改訂版を、opus-critic-a・opus-critic-bへ再度独立に
(互いの指摘を見せずに)再確認させた。**両者とも「設計面には異論なし」**——
round 1のBLOCKING 6件・SIGNIFICANT 9件は全て意図通り解消され、「表面上直したが
指摘の意図とズレている」箇所はゼロと確認された。一方で、**両者が独立に、
round 1の修正自体が新たに導入した誤りを2件ずつ発見**した。「収束するまで
繰り返す」プロセスの価値が実際に発揮された回。

---

## 新規 BLOCKING(両者独立に同一結論)

### N1/R2-1. `token: Option<Vec<u8>>`は新たな型不一致

実シグネチャ(`isekai-ssh/src/native/mux/client.rs:210`)は`token: &[u8]`
(非Option)で、`:232`(`&Frame::Hello { token: token.to_vec(), ... }`)で
無条件に`to_vec()`される。呼び出し元9箇所すべてが実トークンを渡し、`None`に
相当する状態は存在しない。Option化すると`.unwrap()`(新パニック経路)か
`.unwrap_or_default()`(空トークン→owner側`"authentication token mismatch"`
拒否)のどちらかを書く羽目になる。**対処**: `pub token: Vec<u8>`。
(opus-critic-a R2-1・opus-critic-b N-1)

### N2/R2-2. 「14→9引数」は算術ミス、正しくは14→7引数——連鎖してallow判断も逆転

`PtySessionRequest`が吸収するのは8フィールド(`term`/`cols`/`rows`/`host`/
`remote_command`/`want_pty`/`tty_exec`/`token`)。残る個別引数は`conn_read`/
`conn_write`/`stdin`/`stdout`/`stderr`/`resize_rx`の6+`request`1=**7**。
clippyの`too_many_arguments`閾値は7(8以上で発火)なので、**7引数なら
`#[allow(clippy::too_many_arguments)]`を実際に外せる**——round 1時点の
「9引数では外せない」は前提・結論とも逆だった。opus-critic-a自身が
「round 1の私の『14→9』も誤りでした」と自己訂正。傍証: `resume_loop.rs:946`
の`resume_with_backoff_until_deadline`は10引数なのにallowが付いておらず、
このリポジトリでclippyが一度も走っていないことを示す。
(opus-critic-a R2-2・opus-critic-b N-2)

---

## 新規 SIGNIFICANT(opus-critic-a独立発見)

### R2-3. §1.1が§2.5と自己矛盾する事実誤りに退化

改訂で「時間関連4値のうち`max_resume_window`だけがスカラー」と書き換わったが、
実際は4値(`resume_window`/`disconnected_at`/`deadline`/`max_resume_window`)
すべてがスカラーのまま。§2.5の`ResumeDeadlinePolicy`は4値すべてを包む設計
なので、§1.1の記述通りなら§2.5は1フィールドの構造体になるはずで矛盾する。

### R2-4. §2.5の行番号が1行ずれている

`resume_loop.rs:957`ではなく、宣言は`:956`、`notify_on_give_up`の代入は
`:958`(`:957`は関数シグネチャの閉じ括弧)。round 1時点の`:955/:956`という
記述も1行ずれていた。

### R2-5. §3「§2.1はWindows専用経路でrequired checks外」は過大評価

`isekai-ssh/src/native/mod.rs:4-6`が「built and unit-tested on every
platform(Linux CI is cheaper to run than Windows), but only ever *invoked*
on `cfg(windows)`」と明記し、`client.rs`の`#[cfg(test)] mod tests`に
`cfg(windows)`ゲートは無い。§2.1が書き換える8テスト呼び出しはrequired check
の`rust-core-test-linux`で実際に実行される。手動確認の運用負担は不要。

### R2-6. §0改訂履歴の帰属2件が不正確

(a) opus-critic-aの「B1-B4」表記は誤り——B4は§2.4(Compose配置問題)の指摘で
§2.3とは無関係(§0の§2.4行で既に正しく再掲されており二重計上だった)。正しくは
「B1-B3」。(b) §2.1の「CIにclippyジョブが無い」という記述をopus-critic-a/b
両者の指摘としていたが、opus-critic-a自身はこれを指摘しておらず
opus-critic-b単独の指摘(事実自体は正しい)。

---

## opus-critic-b独自の追加指摘

- **§2.4の見落とし**: `cellW`/`cellH`/`themeBgArgb`/`typeface`/
  `effectiveBlinkPhase`は`drawRow`/`redrawDirtyRows`だけでなく
  `gridCache.planRender(...)`/`markRendered(...)`にも個別に渡っており、
  `GridRenderStyle`導入後の配線方針がADRに未記載だった。
- **§2.3縮小版の費用対効果への回答**(依頼した追加質問): 「実施する価値あり。
  コストは1箇所14行のdocコメント追記のみでコード変更ゼロ、得られるのは
  round 1で判明した設計事実が次に読む人に再発しないこと」。ただし
  (1)参照先をADR/レビュー文書(将来archive移動されうる一時文書)ではなく
  恒久ドキュメントにすること、(2)`launch_fingerprint`の除外リストを3つ目の
  理由として追加すること、の2点を改善提案。

---

## 逐一確認された「round 1指摘の解消状況」(両者一致)

round 1のBLOCKING(§2.3統合前提の事実誤認、`None`バリアントの行き場、argv
レンダラ非対称、`Stun`バリアント先行追加、フィールド差異比較表、費用対効果、
core側CI未検証、Compose配置不能、`private`型の可視性エラー、
`Option<Duration>`)とSIGNIFICANT全項目は、上記の新規発見分を除き**全て
意図通り解消**。未決事項5点への結論もADR本文と整合。

## 最終結論(両者一致)

**未収束**。ただし残件は上記の文書上の修正のみで、**設計の作り直しは不要**。
両者とも「上記反映後はround 3で収束する見込み」「設計面には異論なし」と
明言。R2-1(token)とR2-2(引数数・allow判断)はopus-critic-a曰く
「実装時に必ず踏む」問題(前者は認証拒否、後者は誤ったallow指示)のため、
反映前の実装着手は避けるべきとの警告あり。

## 出典一覧

- opus-critic-a: R2-1〜R2-7、round 1指摘の逐一解消確認、設計面への異論なし宣言
- opus-critic-b: N-1・N-2、§2.4見落とし、§2.3費用対効果への回答、round 1指摘の
  逐一解消確認
- 両者が完全独立にN-1/R2-1(token型)・N-2/R2-2(14→7引数)で同一結論に到達
