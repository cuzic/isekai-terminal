---
name: isekai-ssh-windows-deploy
description: main(または指定ref)の isekai-ssh/isekai-pipe を release-build.yml でビルドし、clipwire-exec経由でユーザーのWindows機に配備・検証する。release-build.yml は workflow_dispatch 専用で push 毎には走らないため main へのマージだけでは実機のバイナリは自動更新されない上、`.local/bin` 以外にも実際に使われているコピー(Windows Terminalプロファイルが直接指す同居コピー等)が別途あることがある——「直したはずなのに直っていない」報告のたびに、この二重のギャップをまず疑うために使う。
argument-hint: "[ref(既定main)]"
keywords:
  - isekai-ssh
  - isekai-pipe
  - windows
  - デプロイ
  - release-build
  - clipwire-exec
  - workflow_dispatch
  - バイナリ更新
triggers:
  - isekai-sshをwindowsにデプロイして
  - windows向けに最新版を配って
  - isekai-sshの切断が再発した
  - 直したはずなのに直ってない
  - isekai-ssh --versionが古い
allowed-tools: Bash
---

# isekai-ssh / isekai-pipe の Windows デプロイ

## 背景

`.github/workflows/release-build.yml`(job名 `isekai-ssh / isekai-pipe release build`)は
バージョンタグpushと`workflow_dispatch`でのみ実行され、**通常のmainへのpush/PRマージでは
走らない**。そのため、isekai-ssh/isekai-pipeのバグ修正PRがmainにマージされても、
ユーザーのWindows実機の`.local/bin`配下のバイナリは自動更新されない。

isekai-sshの切断系の不具合報告(「isekai-sshは切断されたがtsshは繋がり続けた」等)を
調査する際、実機に配備されているバイナリが実は数週間前のビルドのままで、直近の修正
(exit-diagnosticsのような「次回の再発を診断可能にする」類の修正を含む)が一切反映
されていない、という状態を2026-09-14に実際に踏んだ(`.claude/rules/always-connects.md`・
`ADR_ISEKAI_SSH_EXIT_DIAGNOSTICS.md`が対象にしている問題そのものの調査中に発生)。
**バグ報告のたびに「配備済みバイナリのgit shaが本当に最新か」を最初に確認する**のが
このスキルの主目的。

## 前提

- `gh` CLIが認証済み(`gh auth status`)。
- `~/.config/clipwire/targets.toml`が存在し、`clipwire-exec`スキルの前提が整っている
  (`CLIPWIRE=~/powershell-clipd/target/release/clipwire`、Windowsホスト名は
  `tailscale status`で解決)。

## 重要: 「実際に使われているバイナリ」は`.local/bin`だけとは限らない

2026-09-15、`.local\bin\isekai-ssh.exe`を最新化しても切断が再発する事案が
あった。原因は、ユーザーが日常使うWindows Terminalプロファイル
(`wt-isekai-profile-full`ターゲットで`settings.json`から確認できる)の
`commandline`が`.local/bin`ではなく**リポジトリの同居コピー**
(`C:\Users\cuzic\scoop\persist\msys2\home\cuzic\isekai-terminal\isekai-ssh`、
中身は`cd`して`./isekai-ssh.exe`をexecするラッパースクリプト)を直接指して
いたため。この同居コピーは`.local/bin`とは別ファイルであり、`.local/bin`側
だけをどれだけ更新してもユーザーが実際に叩くバイナリは古いまま、という
事態が起きる(過去に何度も`isekai-ssh-deploy-localbin-*`と
`update-msys2-colocated-isekai-ssh`の両系統のターゲットが別々に作られていた
のは、まさにこの二重管理に過去のセッションも気づいていた痕跡)。

**デプロイ前に必ず「実際に使われている経路」を特定する**:

```bash
CLIPD_HOST=$(tailscale status | awk '/windows/{print $2}' | head -1)
CLIPWIRE=~/powershell-clipd/target/release/clipwire
# 1. Windows Terminalの各isekai系プロファイルのcommandlineを確認(既存ターゲット
#    wt-isekai-profile-full、無ければ同じPowerShellでsettings.jsonを読む)
CLIPD_HOST=$CLIPD_HOST $CLIPWIRE exec wt-isekai-profile-full
# 2. msys2 bashからの `which isekai-ssh` / `which isekai-pipe` も確認
#    (`.local/bin`の中に拡張子なしコピー`isekai-ssh`が別途あることがあり、
#    `.exe`とは違うファイル/ハードリンクの可能性がある — sha256で必ず比較する)
```

`commandline`が指すパス(直接の`.exe`、またはラッパースクリプトが指す先の
`.exe`)**すべて**を更新対象とする。`.local/bin`はあくまで一例であり、思い込みで
そこだけ更新して終わりにしない。

## 手順

### 1. 現在配備されているバージョンを確認する

既存ターゲット`isekai-ssh-check-version`(無ければ新規登録、下記手順4と同じ要領)で
確認する:

```bash
CLIPD_HOST=$(tailscale status | awk '/windows.*active|windows.*-/{print $2}' | head -1)
CLIPWIRE=~/powershell-clipd/target/release/clipwire
CLIPD_HOST=$CLIPD_HOST $CLIPWIRE exec isekai-ssh-check-version
```

出力の`(<git-sha>)`部分と、デプロイしたい`ref`(既定`main`)の`git log --oneline -1`を
比較する。**このターゲットは`.local/bin`しか見ていない**——上記「重要」節で確認した
実際の起動経路(Windows Terminalプロファイルのcommandline等)が別パスなら、そちらの
`--version`も別途確認すること。**すべての経路で既に一致していれば、この後の手順は
不要**——「デプロイしたつもり」で無駄なビルドを回さない。ズレていたら手順2へ。

### 2. リリースビルドをトリガーする

```bash
cd rust-core  # 不要、リポジトリルートでよい
gh workflow run release-build.yml --ref <ref>   # 既定 main
sleep 8
RUN_ID=$(gh run list --workflow="isekai-ssh / isekai-pipe release build" --branch <ref> --limit 1 --json databaseId --jq '.[0].databaseId')
echo "$RUN_ID"
```

### 3. 完了を待つ

12ターゲットのmatrixビルド(Windows msvc/mingw・Linux musl x2・macOS x2)なので
android-ci-deployより時間がかかる(実測: 数分〜10分程度)。`gh run watch`は
ブロックするので、長時間コマンドを避けたい場合はBashの`run_in_background`か、
数分おきのポーリングで確認する:

```bash
gh run view "$RUN_ID" --json status,conclusion -q '.status + " " + (.conclusion // "-")'
```

`completed success`になるまで待つ。`completed failure`なら
`gh run view "$RUN_ID" --log-failed`で原因を確認し、ここで止まる
(diagnostic目的の再ビルドが失敗している状態でデプロイを強行しない)。

### 4. clipwire-exec の新規ターゲットを登録する

runごとに新しいターゲットが必要(run IDとref先頭8桁のshaが変わるため)。
`<sha8>`はデプロイ対象refの短縮sha(`git rev-parse --short=8 <ref>`)、`<run_id>`は
手順2で取得したもの:

```bash
cat >> ~/.config/clipwire/targets.toml << EOF

[targets.isekai-ssh-deploy-localbin-<sha8>]
dir = 'C:\Users\cuzic\scoop\persist\msys2\home\cuzic\isekai-terminal'
script = """
run(["gh", "run", "download", "<run_id>", "-D", "C:/temp/isekai-release-<sha8>", "--pattern", "*windows-msvc*"]);
run(["powershell", "-NonInteractive", "-Command", "Copy-Item 'C:/temp/isekai-release-<sha8>/isekai-ssh-x86_64-pc-windows-msvc/isekai-ssh-x86_64-pc-windows-msvc.exe' 'C:/Users/cuzic/.local/bin/isekai-ssh.exe' -Force; Copy-Item 'C:/temp/isekai-release-<sha8>/isekai-pipe-x86_64-pc-windows-msvc/isekai-pipe-x86_64-pc-windows-msvc.exe' 'C:/Users/cuzic/.local/bin/isekai-pipe.exe' -Force; Write-Output copied"]);
run(["powershell", "-NonInteractive", "-Command", "Write-Output '--- sha256 ---'; (Get-FileHash 'C:/Users/cuzic/.local/bin/isekai-ssh.exe' -Algorithm SHA256).Hash.ToLower(); (Get-FileHash 'C:/Users/cuzic/.local/bin/isekai-pipe.exe' -Algorithm SHA256).Hash.ToLower(); Write-Output '--- version ---'; & 'C:/Users/cuzic/.local/bin/isekai-ssh.exe' --version; & 'C:/Users/cuzic/.local/bin/isekai-pipe.exe' --version"]);
"""
EOF

CLIPD_HOST=$CLIPD_HOST $CLIPWIRE register isekai-ssh-deploy-localbin-<sha8>
```

**上記「重要」節で`.local/bin`以外の経路(同居コピー等)も見つかった場合は、
そのパスに対しても同様のターゲットを追加する。** 実行中のプロセスがロックしている
可能性のあるパス(同居コピーはWindows Terminalプロファイルから直接execされ続けて
いることが多い)では、`Copy-Item -Force`でいきなり上書きせず、先に`Move-Item`で
`.bak-<timestamp>`へ退避してから新しいバイナリを配置する
(`isekai-ssh-deploy-colocated-*`系ターゲットの実装を参照。Windowsは実行中exeの
直接上書きを拒否することがあるが、リネームしてから新規作成は問題なく通る):

```
[targets.isekai-ssh-deploy-colocated-<sha8>]
dir = 'C:\Users\cuzic\scoop\persist\msys2\home\cuzic\isekai-terminal'
script = """
run_ok(["powershell", "-NonInteractive", "-Command", "$stamp = Get-Date -Format 'yyyyMMdd-HHmmss'; Move-Item isekai-ssh.exe ('isekai-ssh.exe.bak-' + $stamp) -Force; Move-Item isekai-pipe.exe ('isekai-pipe.exe.bak-' + $stamp) -Force -ErrorAction SilentlyContinue"]);
run(["powershell", "-NonInteractive", "-Command", "Copy-Item 'C:/temp/isekai-release-<sha8>/isekai-ssh-x86_64-pc-windows-msvc/isekai-ssh-x86_64-pc-windows-msvc.exe' isekai-ssh.exe -Force; Copy-Item 'C:/temp/isekai-release-<sha8>/isekai-pipe-x86_64-pc-windows-msvc/isekai-pipe-x86_64-pc-windows-msvc.exe' isekai-pipe.exe -Force; Write-Output copied"]);
run(["powershell", "-NonInteractive", "-Command", "Write-Output '--- version ---'; ./isekai-ssh.exe --version; ./isekai-pipe.exe --version"]);
"""
```

Tailscale上でWindowsホストが`offline`表示の場合は`clipd`自体が起動していない/PCが
スリープしている可能性が高い。register/execがタイムアウトしたら、まずユーザーに
Windows機の状態を確認してもらう(自動リトライを繰り返してユーザーの手を煩わせない
——数回失敗したら状況を報告して止まる)。

### 5. 承認を待ってから実行する

**通常のclipwire-execの原則通り**、新規登録したターゲットはユーザーがWindows側で
`clipwire approve isekai-ssh-deploy-localbin-<sha8>`を実行するまで実行できない。
このユーザーは自動承認運用にしていると2026-09-14に明言しているため、登録直後に
execを試してよい(失敗したら承認待ちを案内する):

```bash
CLIPD_HOST=$CLIPD_HOST $CLIPWIRE exec isekai-ssh-deploy-localbin-<sha8>
```

自動承認の運用が確認できていない別ユーザー/別環境では、素直に承認を待つ
(`clipwire-exec`スキル本来の手順に従う)。

### 6. 結果を確認する

exec出力の`--- version ---`が`<ref>`のHEAD shaと一致していることを確認して報告する。
sha256も併記されているので、「本当に新しいバイナリに差し替わったか」をここで
確認できる(`.claude/rules/always-connects.md`が警告する「サイレント再デプロイが
実は中身を見ていない」系の問題と同種の確認)。

**既に起動中のholderプロセス(`isekai-ssh-list-processes`ターゲットで確認できる)
は、ファイルを差し替えても古いバイナリのままメモリ上で動き続ける**(Windowsは
実行中exeのrenameは許すため差し替え自体は成功するが、実行中プロセスの
イメージは差し替わらない)。ユーザーには「新しく開くタブ/セッションから新しい
バイナリが使われる。今開いているタブは閉じて開き直すまで古いまま」と明確に
伝える——「デプロイした」と「今使っているセッションに反映された」を混同しない。

## 参照

- `.github/workflows/release-build.yml`: ビルド本体(12ターゲットmatrix、
  `workflow_dispatch`専用の理由がヘッダコメントに書かれている)
- `~/.claude/skills/clipwire-exec/`: register/approve/execの一般手順
- `ADR_ISEKAI_SSH_EXIT_DIAGNOSTICS.md`: このスキルが解消しようとしている
  「修正はmainに入ったのに実機では再現し続ける」問題の背景
- `android-ci-deploy`スキル: 同種のCI→実機デプロイパターン(Android側)
