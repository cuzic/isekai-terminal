# ADR: 切断中のC→S入力をS→C出力と対称にresumeバッファする

- **Status**: **Proposed**（2026-09-07起草。§3論点1・4は`docs/adr/0006-stun-reestablish-continuity.md`
  のround 1/2レビュー経由で決着済み（2026-09-13、下記参照）。同ADR§3は
  2026-09-13にround 1〜3のレビューを経てApprovedとなり、論点2・3が
  待っていた確定条件は満たされた——ただし論点2（上限サイズ・TTL）・
  論点3（危険なUXへの対処）自体の値はまだ未決のまま。着手可能になった
  ので、実装着手前にopus-adversarial-consultで改めてレビューを挟むこと）
- **対象**（見込み、要精査）: `rust-core/isekai-pipe/src/resume_loop.rs`
  （`C2hReplayBuffer`の隣に置く、§3論点1参照）。当初案にあった
  `rust-core/isekai-transport`・`rust-core/isekai-ssh/src/session.rs`は
  §3論点1の検討の結果採用しないことになった（Android経路との無用な
  共有を避けるため）。
- **入力**: ユーザーとの「mosh的な、切断中の入力バッファリング/画面操作継続」
  検討セッション（2026-09-07）。isekai-terminal（Android/iOS）・isekai-ssh
  （Windows）・比較対象として`tssh`/`tsshd`（trzsz/trzsz-ssh、
  trzsz/tsshd）のアーキテクチャを調査した結果からの派生
- **拘束される既存ルール**: `PLAN.md:982`「対象外: SSHセッションそのものの
  再生成・代理応答・端末状態同期（mosh的なstate syncはやらない）」、
  `PLAN.md:617`「mosh的シームレス再開ではない」、
  `.claude/rules/always-connects.md`

---

## 1. 背景

`isekai-pipe`の既存resume機構は **S→C（サーバー出力）方向のみ非対称に手厚い**:

- `OutputBuffer`（既定4MiB）がPTY出力を保持し、byte-level resumeで
  再接続後に復元される。
- 一方、**C→S（ユーザー入力）方向は`Session::send`が bounded mpsc へ
  `try_send`するだけ**で、バッファが詰まれば`log::warn`した上で
  無警告drop（`session.rs:507-513`、2026-09-07時点で確認）。
  切断中に打たれた新規入力を意図的に貯めて再接続後にflushする
  専用機構は存在しない。

比較のため`tssh`/`tsshd`（trzsz-ssh）を調査したところ、`tsshd`は
サーバー側にシェルプロセス・PTY・VTE相当の画面状態(`te.Screen`)を
**独自に所有するセッションデーモン**として実装しており、それが
真のmosh的耐性の正体だった（`tsshd/session.go`の`sessionContext`）。
これは`PLAN.md:982`が明記する「対象外」の領域そのものであり、
`isekai-pipe`はこの方向には進まない（isekai-pipeは薄いトランスポート
中継のままにしたい、というユーザー判断）。

本ADRが扱うのはそれとは別の、**既存resumeプロトコルの対称化**という
より小さいスコープ:「サーバー側が本物の`sshd`と直接話す」という
現行構造を一切変えず、クライアント側（isekai-ssh/isekai-transport）
だけで完結する改善。

## 2. 狙い

切断中にユーザーが入力したバイト列を、既存のRESUMEプロトコル
（byte-level resume、既定resume window 10日）に載せて、再接続確立後に
そのまま送信できるようにする。S→C出力バッファの鏡像を、C→S入力にも
持たせるイメージ。

## 3. 検討すべき論点（未決）

1. **キューの置き場所**: **`docs/adr/0006-stun-reestablish-continuity.md`§7.1で
   結論済み**——`isekai-ssh`側のsession層でも汎用`isekai-transport`層
   でもなく、`isekai-pipe/src/resume_loop.rs`内、`C2hReplayBuffer`の隣。
   理由: resumeのoffsets管理（`C2hReplayBuffer`/
   `helper_committed_offset`）が既にここにあり、session_idもここが
   唯一の保持者。`isekai-transport`に置くとAndroid経路
   （`ReattachableStream`経由、別のresume駆動構造）と無理に共有する
   ことになる。キューのスコープも`run_resume_loop`が保持する
   `session_id`（`resume_loop.rs:1260`）とし、connection/transport
   オブジェクトには紐づけない（同ADR§7.1参照、cross-family resume
   導入後は「同一トランスポートファミリ内の再接続」ではなく
   「同一session_idでの再接続、ファミリ不問」がflush境界になるため）。
2. **上限サイズ・TTL**: resume windowと揃えるか、もっと短い上限にするか。
   S→C側の`OutputBuffer`（4MiB）との対称性を取るか、入力は本質的に
   小さいので別のポリシーにするか。**この決定は`docs/adr/0006-stun-reestablish-continuity.md`
   §3の採否を待ってから行う**——下記論点3・5参照。
3. **危険なUX**: 切断中に打った内容（危険なコマンド含む）が、長時間後の
   再接続時に無警告で実行される事故をどう防ぐか。最低限、flush時に
   何らかの可視化（stderrへのログ等）が要りそう。isekai-sshはUIを
   持たないため、通知手段の選択肢が限られる点に注意。
   **`docs/adr/0006-stun-reestablish-continuity.md`§3（cross-family
   resume-preserving fallback）が実装されると、サイレントに再接続が
   成功する窓がSTUNの120秒（`STUN_RESUME_GIVE_UP_WINDOW`）からrelayの
   resume grace（既定10日）へ伸びる**ため、本論点のリスク予算そのもの
   が変わる。「スコープが重なる」のではなく「あちらの決定がこちらの
   安全要件の前提を変える」関係——本ADRの上限サイズ・TTL・可視化
   ポリシーは、同ADR§3の採否が確定してから決める(順序が逆だと
   決め直しになる)。
4. **既存`ReplayBuffer`/`ClientResumeState`との役割分離**: **結論済み**
   （`docs/adr/0006-stun-reestablish-continuity.md`§7.1参照）——両者は「送信済み
   だが未ACKのバイトを再送する」ためのもので、本ADRが扱う「そもそも
   オフラインで送信すらしていない新規入力」とは性質が異なる。flush
   地点を`resume_loop.rs:594 replay_and_advance`と同じ場所にし、
   「先に未ACK再送、次に未送信flush」という順序を固定すれば、
   レイヤーは自然に分かれる（混線するのは両方を1つのバッファに
   詰め込もうとしたときだけ）。
5. **`docs/adr/0006-stun-reestablish-continuity.md`との関係**: 当初「STUN P2P
   経路でフルre-establishが発生すると新セッション扱いになりresume
   連続性自体が失われるため、本ADRの保証範囲もそれに合わせて限定
   される」と書いていたが、同ADRのround 1レビュー（2026-09-13）で
   前提が更新された——**クライアント側アドレスだけが変わる圧倒的
   多数のケースでは、cross-family resume-preserving fallbackにより
   `ssh(1)`を再起動せず連続性を保てる見込み**（同ADR§3）。この場合、
   本ADRの入力キューの保証範囲も連続性が保たれるケースまで自然に
   拡張される。「双方同時にアドレスが変わる真の再ランデブー」（同ADR
   §4.2、非目標）のケースのみ、引き続き新セッション扱いで本ADRの
   保証範囲外となる。

## 4. 次のステップ

`docs/adr/0006-stun-reestablish-continuity.md`§3の実装・確定を待ってから
（論点2・3がその決定に依存するため）、実装着手前に
`opus-adversarial-consult`スキルで設計相談する
（特に論点3のUX安全性、論点4のReplayBufferとの混線リスク）。
