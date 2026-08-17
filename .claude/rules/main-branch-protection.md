# mainブランチ保護(branch protection)

`main`ブランチにclassic branch protection(rulesetではない)を導入している
(2026-08-17、項目3 Phase 1〜4)。GitHub Free でも public リポジトリなら無償で使える。

## 現状(Phase 4完了時点)

```json
{
  "required_status_checks": {
    "strict": false,
    "checks": [
      {"context": "android-unit-test"},
      {"context": "rust-core-test-linux"},
      {"context": "android-uniffi-drift"},
      {"context": "lockfile-drift"},
      {"context": "room-migration"}
    ]
  },
  "enforce_admins": false,
  "required_pull_request_reviews": null,
  "restrictions": null,
  "required_linear_history": false,
  "allow_force_pushes": false,
  "allow_deletions": false,
  "block_creations": false,
  "required_conversation_resolution": false,
  "lock_branch": false,
  "allow_fork_syncing": true
}
```

- **PRレビュー必須化はしない**(`required_pull_request_reviews: null`)。実質1人+AI
  エージェント体制のため、レビュー必須化は開発速度を殺すだけで安全性の実利が薄い。
- **`enforce_admins: false`**(現在の状態)。全員adminトークンでpushする運用のため、
  この状態では実は誰でもrequired checksをバイパスして直pushできてしまう
  (protectionは「PR経由でマージするときのゲート」としてのみ効き、直pushそのものは
  まだ止めない)。1週間の観測期間を経てから`true`に昇格する設計(下記Phase 5参照)。
- **`allow_force_pushes: false` / `allow_deletions: false`**は維持(mainへの
  force-push/削除は常に禁止、enforce_adminsの状態に関わらず有効)。

## required 5本の一覧と意味

| context | 元job id / ワークフロー | 意味 | 実行時間目安 |
|---|---|---|---|
| `android-unit-test` | `android-test-check.yml`の`test` | `./gradlew :android:testDebugUnitTest`(Robolectric/JVM) | 〜17分(pr-path-gate経由で無関係PRは20〜30秒) |
| `rust-core-test-linux` | `rust-core-test-check.yml`の`test` | rust-core全体の`cargo nextest run --workspace`(Linux) | 〜数分〜十数分 |
| `android-uniffi-drift` | `android-uniffi-drift-check.yml`の`drift-check` | UniFFI Kotlinバインディングのdrift検知 | 〜4分(pr-path-gate経由で無関係PRは20〜30秒) |
| `lockfile-drift` | `lockfile-drift-check.yml`の`check` | `cargo metadata --locked`によるCargo.lock整合性検証 | 数秒〜十数秒 |
| `room-migration` | `room-migration-check.yml`の`check` | Room migration番号の整合性検証 | 数秒〜数十秒 |

5つとも元は各ワークフローのjob id(`test`/`drift-check`/`check`等)がGitHub上の
check-run context名になっていたが、`android-test-check.yml`と`rust-core-test-check.yml`
が両方job id `test`を使っており**context名が衝突していた**(2026-08-17発見)。
同様に`lockfile-drift-check.yml`と`room-migration-check.yml`も両方job id `check`を
使っており、もう1つの実際の衝突だった。Phase 1で各jobに明示的な`name:`を付けて
これを解消した。**この5つのcontext名はbranch protectionのrequired status checkとして
直接参照されているため、対応するワークフローファイルの`name:`を変更する場合は、
必ず同時に`gh api -X PUT repos/cuzic/isekai-terminal/branches/main/protection`で
protection設定側の`checks[].context`も更新すること**(更新を忘れると、その
contextは二度と緑にならず恒久pendingでmainへのマージが詰まる)。

対象外(required化しなかったもの):
- `rust-core-test-check.yml`の`test-macos`/`test-windows`: 将来のrequired候補として
  維持しているが、まだ安定性が未検証(Phase 6参照)。既に`name: rust-core-test-macos`/
  `name: rust-core-test-windows`は付与済み(2026-08-17、pr-path-gate信頼性修正の
  ついでに前倒しで対応——job id(`test-macos`/`test-windows`)そのままでは`test`job
  のときと同じcontext名衝突を将来招きかねないため)。
- `ios-*-check.yml`各種、`ios-app-build-check.yml`(31分)、
  `ios-ssh-vertical-slice-check.yml`(27分): コスト対効果が悪いため対象外のまま
  (Phase 6でも`ios-app-build`/`ios-ssh-vertical-slice`は除外予定)。
- `fdroid-build-check.yml`: `assembleRelease`のビルド確認、60〜100分と重すぎる。

## `strict: false`にした理由

`strict: true`(「マージ前にブランチが最新であること」を要求)は、並列worktree/
エージェント運用で複数PRが同時に走る際、1本マージされるたびに他の全PRが
「ブランチが古い」判定になり、再実行(rebase/merge + 再度required checksを
待つ)を延々と繰り返す「再実行地獄」を招く。このリポジトリは並列PR運用が
常態(`.claude/rules/parallel-worktree-agent-operations.md`参照)なため、
`strict: false`(ブランチ最新性は問わず、対象PRのhead SHA自身でrequired
checksが緑であればマージ可)を選んだ。

## Phase 5: `enforce_admins`昇格の手順(まだ実施していない)

1週間、以下を観測してから昇格する:
- ドキュメントのみのPRで5本すべて緑が付くか(pr-path-gate経由の恒久pending対策が
  機能しているか)。
- `gh pr merge --auto`が実際に動作するか(Phase 3の`allow_auto_merge`設定と
  組み合わせて)。
- `rust-core-test-linux`のgreen rate(75%を切るなら先に安定化を優先し、昇格を
  延期する)。

観測後、問題なければ:

```bash
# mainの最新コミットでrequired 5本が全て緑であることを確認してから
gh api -X POST repos/cuzic/isekai-terminal/branches/main/protection/enforce_admins
```

昇格後の運用フローは次のいずれかを既定にする:
- PR + `gh pr merge --auto --squash --delete-branch`(Phase 3で有効化した
  `allow_auto_merge`を使う)。
- 「PRでrequired checksが緑になったSHAをそのまま`main`へff-push」
  (直pushそのものは`enforce_admins: true`後も、required checksを実際に
  満たしたコミットであれば技術的には可能——ただしGitHub UIの「Require status
  checks to pass before merging」はPRのマージボタン経由のマージにのみ強制され、
  裸の`git push`はチェックを経由せず素通りする点に注意。`enforce_admins: true`
  は「管理者もPR経由のマージルールに従わせる」設定であり、「PR以外の経路での
  push自体を止める」設定ではない。裸pushを本当に禁止したい場合は
  `restrictions`(push可能なactor/teamの制限)が別途必要——今回は導入していない)。

昇格後、`.claude/rules/parallel-worktree-agent-operations.md`に「5. main反映は
PR+auto-mergeを既定にする」を追記する。

## break-glass手順

`enforce_admins: true`昇格後、緊急でrequired checksを無視してmainへ反映しなければ
ならない場合(例: CI基盤自体が壊れていてすべてのcheckが赤/pendingのまま動かせない):

```bash
# 1. 一時的にenforce_adminsを外す
gh api -X DELETE repos/cuzic/isekai-terminal/branches/main/protection/enforce_admins

# 2. 必要な変更をpush(このリポジトリは全員adminトークンでpushする運用なので、
#    enforce_admins=falseの間はrequired checksを満たさないコミットも直push可能)
git push origin HEAD:main

# 3. 必ず戻す
gh api -X POST repos/cuzic/isekai-terminal/branches/main/protection/enforce_admins
```

**手順3(戻す)を省略しない**。外しっぱなしにすると、Phase 5で昇格した保護が
事実上無効化されたまま気づかれずに運用が続くリスクがある。

## Phase 6(将来、任意)

green rateが95%以上安定したら`ios-logic-linux`→`rust-core-test-windows`→
`rust-core-test-macos`の順でrequiredに追加する。`rust-core-test-check.yml`の
`test-macos`/`test-windows`両jobには既に`name:`が付与済み(上記参照)なので、
Phase 6実行時に改めて`name:`追加の作業をする必要はない——そのまま
`checks[].context`に`rust-core-test-macos`/`rust-core-test-windows`を追加するだけでよい。
`ios-app-build`(31分)/`ios-ssh-vertical-slice`(27分)はコスト対効果が悪いため
対象外のまま維持する。

## 参照実装

- `.github/workflows/android-test-check.yml` / `rust-core-test-check.yml` /
  `android-uniffi-drift-check.yml` / `lockfile-drift-check.yml` /
  `room-migration-check.yml`: 各`jobs.<id>.name:`(context名の実体)
- `.github/actions/pr-path-gate/action.yml`: path-filter付きワークフローの
  恒久pending対策(base.sha/github.shaのローカルgit diffによるERE照合。
  当初`gh pr diff --name-only`を使っていたが、大規模PRでのdiff切り詰め・
  `synchronize`時のSHAレースのリスクがあったため2026-08-17にgit diffベースへ
  置き換えた)
- `/home/cuzic/.claude/plans/flickering-wondering-petal.md` 項目3セクション:
  この導入の元になった設計プラン全文
