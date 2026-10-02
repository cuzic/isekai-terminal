# ADR: isekai-ssh（Windows）にローカルscrollbackバッファを持たせる

- **Status**: **Proposed**（2026-09-07起草。設計相談・opus-adversarial-consultは
  未実施。実装着手前にレビューを挟むこと）
- **対象**（見込み、要精査）: `rust-core/isekai-ssh/src/native/`
  （PTY出力をconsoleへ流す経路）
- **入力**: ユーザーとの「mosh的な、切断中の入力バッファリング/画面操作継続」
  検討セッション（2026-09-07）。isekai-terminal-core（Android/iOS）・
  isekai-ssh（Windows）・比較対象として`tssh`/`tsshd`のアーキテクチャを
  調査した結果からの派生
- **ユーザー決定事項**（2026-09-07）: 「isekai-pipeは薄くありたいが、
  isekai-sshはこの程度の逸脱なら許容してもいい」——本ADRのスコープは
  `isekai-pipe`（サーバー側含む）を一切変えず、`isekai-ssh`クライアント
  単体で完結させることが前提
- **拘束される既存ルール**: `PLAN.md:982`「対象外: SSHセッションそのものの
  再生成・代理応答・端末状態同期（mosh的なstate syncはやらない）」——
  本ADRはこの判断と抵触しないことを確認しながら設計する（サーバー側の
  セッション所有デーモン化はしない）

---

## 1. 背景

- **isekai-terminal-core（Android/iOS）**: 自前のSSHクライアント（russh）
  に加え、`terminal.rs`のVTEベース`Terminal`（グリッド・scrollback・
  Sixel等）を自前で持ち、サーバーからのバイトを都度パースしてローカルに
  永続状態として保持している。
- **isekai-ssh（Windows native）**: SSHプロトコル自体はrusshで自前終端
  するが、PTY出力はConPTY経由でそのままWindows console（Windows
  Terminal等）に流すだけで、画面状態を一切保持しない。
- 比較調査した`tssh`/`tsshd`（trzsz-ssh）は、サーバー側の`tsshd`
  デーモンがシェル・PTY・VTE相当の画面状態(`te.Screen`)を独自に所有する
  ことでmosh的耐性を実現していたが、これは`PLAN.md:982`の「対象外」
  判断（サーバー側でのセッション代理・端末状態同期）に抵触するため
  isekai側では採らない、という結論に既に至っている。

本ADRはそれとは別の、**クライアント側（isekai-sshプロセス自身）に
限定した、より小さいスコープ**の話。isekai-terminal-core相当の
scrollback保持機能を、サーバー側を一切変えずisekai-sshにも持たせられ
ないか。

## 2. 狙い

isekai-ssh（Windows native）が、consoleへ書き出す前の受信済み生バイトを
一定量ローカルにリングバッファし、切断中でも直前までの出力を見返せる
ようにする。サーバー側（`isekai-pipe serve`、実`sshd`）は一切変えない。

## 3. 検討すべき論点（未決、価値の検証を含む）

1. **そもそも価値があるか**: Windows Terminal自体が既に自分の
   scrollbackを持っている。isekai-ssh側で別途保持する意義があるのは
   「Windows Terminalのウィンドウ/タブを閉じた後の復元」等、Windows
   Terminal側のscrollbackでは救えないケースに限られる可能性がある。
   ここが本当に困りごとに対応しているか、着手前にユーザーとすり合わせ
   が要る。
2. **何を保持するか**: 生バイトのみをリングバッファするか、
   isekai-terminal-core同様にVTEパース済みのgrid/scrollbackまで持つか。
   後者はisekai-terminal-coreとのコード共有可能性がある一方、
   isekai-sshの「薄いネイティブクライアント」という設計意図からの
   逸脱量が増える（ユーザーは「この程度の逸脱は許容」と判断済みだが、
   VTEフルパーサーまで持つのはその「程度」を超える可能性がある——
   実装時に改めて規模感を確認すべき）。
3. **見返す手段の提供方法**: isekai-ssh自体はGUIを持たないCLIなので、
   「見返す」操作をどう提供するか（別サブコマンドでバッファをdumpする、
   ローカルファイルへ継続的にログ出力する、等）。
4. **保持期間・上限**: メモリ上のみか、プロセス終了後も残すか
   （後者はディスクへの永続化が必要になり、機密性のある端末出力を
   ディスクに残すセキュリティ上の考慮も必要になる）。

## 4. 次のステップ

論点1（そもそもの価値）をユーザーとすり合わせてから、
`opus-adversarial-consult`スキルで設計相談する。
