# GitHub Actions実行環境への対話的デバッグ(tmate)

CIでしか再現しない(ローカルでは再現しない)不具合の原因調査に、実際のGitHub Actions
実行環境へSSHで対話的に入って調べる手順。`rust-core-test-check.yml`の
`real_sshd_multihop_bootstrap_e2e.rs`断続的失敗の原因究明(isekai-pipeネイティブ
debugバイナリがテスト実行中に同期ビルドされ、他の並行実行中のE2Eテストからリソースを
奪っていた問題)で実際に使い、有効だった。

## 前提となる制約(なぜ単純にログを見るだけでは足りないか)

- `gh run view <run-id> --log`は**ジョブが完了してから**でないとログを返さない
  (`run is still in progress; logs will be available when it is complete`)。
- `gh api repos/OWNER/REPO/actions/jobs/<job-id>/logs`も同様に、実行中ジョブでは
  `BlobNotFound`(404)になる。GitHub Actionsは実行中ジョブの生ログをREST API経由で
  ストリーミング公開していない(Web UI自体は別の内部APIを使っている)。
- そのため、**ジョブが失敗してから事後にログを読むだけ**では、タイミング依存の
  問題やビルド途中の状態を観察できない。実行中の環境そのものに入る必要がある。

## 手順

### 1. ワークフローにtmateステップを追加する(detachedモード必須)

```yaml
permissions:
  contents: read
  pull-requests: write  # 接続文字列をPRコメントとして中継するために必要

# ...

- name: DEBUG tmate session (detached)
  uses: mxschmitt/action-tmate@v3
  with:
    detached: true

- name: DEBUG relay tmate connection string via PR comment
  env:
    GH_TOKEN: ${{ github.token }}
  run: |
    set -x
    CONN=""
    for i in $(seq 1 15); do
      CONN=$(/tmp/tmate/tmate -S /tmp/tmate.sock display -p '#{tmate_ssh}' 2>&1 || true)
      case "$CONN" in ssh\ *) break ;; esac
      sleep 2
    done
    {
      echo "DEBUG tmate connection:"
      echo "\`$CONN\`"
    } > /tmp/debug-tmate-body.md
    gh pr comment ${{ github.event.pull_request.number }} --body-file /tmp/debug-tmate-body.md

- name: DEBUG keep runner alive for interactive session
  run: sleep 1200
```

**`detached: true`が必須の理由**: 既定(非detached)のtmateステップは、接続文字列を
*自分自身のステップログに*出力してから接続を待ってブロックする。上記の制約により
そのログを実行中に読めないため、`detached: true`にしてジョブを先に進め、
**次のステップで自分で接続文字列を取り直してPRコメントとして中継する**。
PRコメントは通常のREST API(ログのストリーミング制約を受けない)で取得できる。

**ハマりどころ**:
- `tmate`バイナリはPATHに乗っていない。実体は`/tmp/tmate/tmate`、ソケットは
  `/tmp/tmate.sock`(action-tmateの実装詳細、将来のバージョンで変わりうる。
  変わっていたら`ps aux | grep tmate`で実際の起動コマンドラインを確認する)。
- `mxschmitt/action-tmate@v3`はデフォルトで「利用方法の説明」画面を表示し、
  何かキーを押すまで実際のシェルに入れない(後述、pexpect側でハンドリングする)。
- `DEBUG keep runner alive`のsleepを入れないと、`detached`モードでは後続の
  実ステップ(今回で言えば`cargo test --workspace`)がそのまま走ってジョブが
  終わってしまい、tmateセッションごとRunnerが消える。ゆっくり調査したいだけの
  場合は`sleep <秒数>`で十分な時間を確保する。

### 2. push→PRコメントをポーリングして接続文字列を取得する

```bash
gh pr view <PR番号> --json comments -q '.comments[-1].body'
```
`DEBUG tmate connection: \`ssh <token>@<host>\`` の形式で出力される。

### 3. 接続文字列でSSH接続する(素の`ssh`ではなくpexpect経由が必要)

`ssh user@host <<'EOF' ... EOF`のような素のheredocパイプでは**動作しない**
(tmateは対話端末を要求し、素のheredocでは案内画面の「Press <q> or <ctrl-c> to
continue」プロンプトを越えられずタイムアウトする)。`uv run --with pexpect`で
Pythonスクリプトを書いて能動的にpump(read)する:

```python
import pexpect

child = pexpect.spawn(
    "ssh -o StrictHostKeyChecking=no -o ConnectTimeout=15 <token>@<host>",
    timeout=60,
)

def run(cmd, seconds=8):
    child.sendline(cmd)
    try:
        child.expect(pexpect.TIMEOUT, timeout=seconds)  # 何もmatchしなくてもreadが走る
    except Exception as e:
        print(f"[err] {e}")
    print(child.before.decode(errors="replace"))

run("clear; echo READY", 3)
run("cd rust-core && cargo test -p isekai-ssh --test <test-name> -- --nocapture", 60)
child.close(force=True)
```

**最大のハマりどころ**: `pexpect`は`expect()`(または`read_nonblocking()`)を
明示的に呼ばない限り、接続が生きていても相手からのデータを読み取らない。
`child.send(...)`してから`time.sleep(...)`するだけのコードは、送信はできるが
**応答が一切ログに残らない**(送った内容がそのままecho backされて見えるだけで、
実際にはリモート側の出力を全く読んでいない)。必ず`child.expect(pexpect.TIMEOUT,
timeout=N)`を挟んで能動的にpumpすること。

### 4. 調査が終わったら必ず後片付けする

- `gh run cancel <run-id>` でジョブを止める(sleepが切れるまで待たない)。
- ワークフローファイルからDEBUG系ステップ・`pull-requests: write`権限を
  全て削除し、元の状態に戻すコミットをpushする。

## 実例

`.github/workflows/rust-core-test-check.yml`のPR #5(`fix-rust-core-test-ci`
ブランチ)で、`real_sshd_multihop_bootstrap_e2e.rs`が4回連続でCIのみ
"jump host unreachable" / "Broken pipe (os error 32)"で失敗した際にこの手順を使った。
実行環境へ入って手動で同じテストを走らせたところ、`isekai-pipe binary not found ...
building it now`というテスト自身のログが出ることを発見し、`target/debug/isekai-pipe`
がテスト実行中に同期的にビルドされていた(重量級コンパイルが並行E2Eテストの
CPUを奪っていた)ことが根本原因と特定できた。ローカル開発機では既にビルド済み
だったため一度も再現しなかった。
