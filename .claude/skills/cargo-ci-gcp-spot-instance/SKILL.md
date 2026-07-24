---
name: cargo-ci-gcp-spot-instance
description: rust-core(isekai-terminal-core/isekai-pipe/isekai-ssh等)のビルド・テスト・cargo-mutants、android/のGradleビルド・ユニットテストが、このサンドボックス上の他エージェントとのCPU競合(load average確認で判明)のせいで実用にならないくらい遅いときに、GCPの専用Spotインスタンスを立てて隔離環境でビルド/テストを回す。terraformで冪等に構築、ビルドキャッシュ(cargo registry/target・Android SDK/NDK・Gradle依存)はinstanceと別ライフサイクルの永続ディスクに置く。iOS(ios/)は対象外(macOS VMがGCPに無い、既存のGitHub Actions macosランナーを使う)。
argument-hint: "[回したい重いコマンド(例: cargo mutants -p isekai-terminal-core -f orchestrator.rs、./gradlew testDebugUnitTest)]"
keywords:
  - GCP
  - spot instance
  - terraform
  - cargo-mutants
  - gradle
  - android build
  - CPU競合
  - ビルドが遅い
  - load average
triggers:
  - ビルドが遅すぎる
  - CPU競合
  - cargo-mutantsが終わらない
  - gradleが終わらない
  - GCPでビルドしたい
  - スポットインスタンス
allowed-tools: Bash, Read, Write, Edit
---

# GCP Spotインスタンスでの重いビルド/テスト実行(rust-core・isekai-ssh・android/)

## いつ使うか

rust-core(`isekai-terminal-core`/`isekai-pipe`/`isekai-ssh`等)の`cargo build`/
`cargo test`/`cargo mutants`、または`android/`の`./gradlew`が「異常に遅い」と
ユーザーや自分が感じたら、まず`uptime`でload averageを見て、CPUコア数(`nproc`)に
対して明らかに過負荷(このサンドボックスは他の複数エージェントが同時に動くため、
load averageがコア数の2〜3倍になることが実際にあった)かどうかを確認する。

- **軽い検証(1回のビルド確認・少数のテスト実行)** → まず`gh workflow run`/`gh run watch`で
  GitHub Actions上で検証する方が手軽(このリポジトリはローカルディスクも他エージェントとの
  競合で逼迫しがちなので、軽い検証にこのスキルの重量級インフラは不要)。
- **長時間・多数回のビルドを繰り返す重いジョブ**(cargo-mutantsの全ミュータント走査、
  workspace全体の繰り返しテストループ、Androidの全ユニットテスト+アセンブル等)で、
  GitHub Actionsのjob時間や並列度では非効率、かつローカルは他エージェントとの競合で
  完走の見込みが立たない場合にこのスキルを使う。
- **`android/`のビルド/テストは、2026-07-24時点でこのスキル(GCP spot instance)を
  デフォルトにする案を一度採用したが、同日中にOpusレビュー+Web調査の結果
  「過剰設計」と判断して撤回した**。iOSが既存の`macos-26`ランナーを使っているのと
  同じ発想で、AndroidもGitHub-hosted runner(public repoなので無料、Android SDK/NDK
  プリインストール済み)へビルドを丸投げする方針に変更している
  (`.github/workflows/build-android.yml` + `android-ci-deploy`スキル参照)。
  このスキル(GCP Spot VM)は、GitHub Actionsのjob時間・並列度では非効率な
  **真に重いジョブ**(cargo-mutantsの全ミュータント走査、workspace全体の繰り返し
  テストループ等)専用のbreak-glassオプションとして位置づける。「Androidのビルドが
  遅い」程度の理由では、まず`android-ci-deploy`スキルを試すこと。
- **iOS(`ios/`、Xcode/Swiftビルド)は対象外**。GCPにmacOS VMは無いので、
  引き続き既存のGitHub Actions `macos-26`ランナーを使う(詳細はステップ5参照)。

## 全体の流れ

1. (初回のみ)GCPプロジェクトをbootstrapする
2. Spot preemptionが少ないリージョン/ゾーンを実データで選ぶ
3. `~/cargo-ci-gcp-spot-instance`にterraformを配置し`apply`でinstance一式を作る
4. instanceにSSH(IAP tunnel)して依存ツールを入れる(初回のみ、以後は永続ディスクに残る)
5. リポジトリのソースを転送する
6. 重いジョブをリモート側で`nohup`+`disown`してdetach起動する
7. SSH越しに進捗をpollする(**preemptionで落ちていたら再起動して`--iterate`等で再開する**)
8. 結果を回収する
9. (オプション)`build-android.yml`から使いたい場合は、GitHub Actions self-hosted
   runnerとしてephemeral登録する(恒常的なrunnerにはしない)
10. instanceだけ削除する(キャッシュディスク・ネットワークは残す)

## 0. どのGCPプロジェクト/アカウントを使うか、ユーザーに確認する

`gcloud auth list`で認証済みアカウントを確認し、`gcloud projects list --account=<personal>`
で既存プロジェクトを確認する。isekai-terminal専用のプロジェクトが無ければ、
**ユーザーに確認せず勝手に決めない**(課金が発生するため)。特に:
- どのプロジェクト/新規作成するか
- どの billing account を使うか(`gcloud billing accounts list`)
- インスタンスのスペック方針(コスト重視か、terminateされにくさ重視か)
- 完了後インスタンスを自動削除するか

を、質問ツールでユーザーに確認してから進める(2026-07-24、実際にこの手順でユーザーに
確認してから進めた)。

## 1. (初回のみ)GCPプロジェクトのbootstrap

プロジェクト自体の作成とbilling account linkは**terraformの管理外**にしている
(`google_project`リソースをterraformで持つと、`terraform destroy`が誤って
プロジェクトごと消す事故につながるため)。

```bash
gcloud projects create <PROJECT_ID> --name="<表示名>" --account=<personal-account>
gcloud billing projects link <PROJECT_ID> --billing-account=<BILLING_ACCOUNT_ID> --account=<personal-account>
```

## 2. Spot preemptionが少ないリージョン/ゾーンを実データで選ぶ

**「旧世代マシンタイプの方がpreemptionされにくい」という一般論だけで区域を決めない。
実際に2026-07-24、`us-central1-a`で旧世代N1(`n1-highcpu-32`)を使ってもわずか
26分でpreemptされた実例がある。** マシンタイプの世代だけでなく、リージョン/ゾーン
ごとの実際の需給が支配的要因なので、GCPの Capacity advisor(`gcloud beta compute
advice`)で実測してから決める。

### 前提: gcloudのバージョン

`gcloud beta compute advice capacity`/`capacity-history`サブコマンドは比較的新しい
機能で、2026-07-24時点でこのリポジトリのサンドボックスに入っているシステムの
`google-cloud-cli`(548.0.0)にはまだ入っていなかった(577.0.0では確認できた)。
`sudo apt-get --only-upgrade`できない環境(このサンドボックスがそう)では、
ユーザーローカルに新しいgcloudを展開して使う(既存の認証設定・プロジェクト設定は
`CLOUDSDK_CONFIG=~/.config/gcloud`でそのまま使い回せる、システム側とは独立なので
システムのgcloudを壊す心配はない):

```bash
mkdir -p ~/.local/gcloud-sdk
curl -sL https://dl.google.com/dl/cloudsdk/channels/rapid/downloads/google-cloud-cli-linux-x86_64.tar.gz \
  | tar -xz -C ~/.local/gcloud-sdk --strip-components=1
CLOUDSDK_CONFIG=~/.config/gcloud ~/.local/gcloud-sdk/bin/gcloud components install beta --quiet
```

展開後は850MB程度になる。**ディスクが逼迫している場合、使い終わったら
`rm -rf ~/.local/gcloud-sdk`で消してよい**(このスキルの目的においては
一時的な調査用ツールであり、常駐させる必要はない)。

### 実際に使うコマンド

このスキルに同梱の`find-low-preemption-region.sh`が候補リージョンを一括比較する:

```bash
GCLOUD_BIN=~/.local/gcloud-sdk/bin/gcloud \
  .claude/skills/cargo-ci-gcp-spot-instance/find-low-preemption-region.sh \
  <PROJECT_ID> n1-highcpu-32 <personal-account> \
  us-central1 us-east1 us-east4 us-west1
```

内部では2つのAPIを叩いている:
- `gcloud beta compute advice capacity-history --types=PREEMPTION --region=<region>`:
  過去30日の日次preemption率(0.0〜1.0)
- `gcloud beta compute advice capacity --provisioning-model=SPOT --size=1 --region=<region>`:
  現在のobtainability score(0.0〜1.0、高いほど今すぐ確保しやすい)とおすすめゾーン

### 2026-07-24に`n1-highcpu-32`で実測した結果(参考値、変動するので都度取り直すこと)

| region | avg preemption率(30日) | obtainability |
|---|---|---|
| us-west1 | 0.91(**明確に避けるべき**) | 0.9 |
| us-central1 | 0.36(実際にこれを使って26分でpreemptされた) | 0.9 |
| us-east1 | 0.30 | 0.9 |
| us-east4 | **0.14(この中で最良)** | 0.9 |

obtainabilityはどのregionも同じ0.9だったので、**preemption率の低さだけで`us-east4`
(推奨ゾーン`us-east4-c`)を選ぶのが合理的**だった。`variables.tf`のデフォルトも
この結果を反映して`us-east4`/`us-east4-c`にしてある。

## 3. terraformを配置してapply

`~/cargo-ci-gcp-spot-instance`が無ければ、このスキルの`terraform/`配下を丸ごとコピーする:

```bash
mkdir -p ~/cargo-ci-gcp-spot-instance
cp .claude/skills/cargo-ci-gcp-spot-instance/terraform/*.tf ~/cargo-ci-gcp-spot-instance/
cp .claude/skills/cargo-ci-gcp-spot-instance/terraform/startup-script.sh ~/cargo-ci-gcp-spot-instance/
cp .claude/skills/cargo-ci-gcp-spot-instance/terraform/README.md ~/cargo-ci-gcp-spot-instance/
```

構成の中身(詳細は`terraform/README.md`・`terraform/main.tf`参照):
- 専用VPC + IAP経由SSHのみ許可するfirewall(22番は世界に非公開)+ Cloud NAT(外部IPなしで
  apt/cargoのダウンロードだけ通す)
- Spotインスタンス本体(`google_compute_instance.builder`)
- **ビルドキャッシュ用の永続ディスク**(`google_compute_disk.cargo_cache`、
  `prevent_destroy = true`)。instanceとライフサイクルを分離しているのが肝で、
  `terraform destroy -target=google_compute_instance.builder`でinstanceだけ消しても
  このディスクは残る

`terraform.tfvars`は無いのでデフォルト値(`variables.tf`)がそのまま使われる。
プロジェクトIDを変えた場合は`-var="project_id=..."`で上書きするか`terraform.tfvars`を作る。

### `~/cargo-ci-gcp-spot-instance`は複数エージェントで共有され得る

このサンドボックスは複数の`claude`セッションが同時に動く運用のため、**この
ディレクトリ・このterraform stateを、isekai-terminalとは無関係な別プロジェクトの
Rustビルド(2026-07-24時点で実例: `rust-nicola`/`awase`)が既に同時に使っていた**
(プロジェクト/ネットワーク/firewall/NATは共用しつつ、`rust_nicola_*`接頭辞の変数・
別instance・別cache diskで名前空間を分離する形)。このディレクトリで作業する前に
必ず`main.tf`/`variables.tf`を`git diff`相当(ここはgit管理下ではないので直接
`cat`/`diff`)で確認し、**自分が把握していないリソースが増えていても勝手に消したり
上書きしたりしない**こと。`terraform apply`/`destroy`は他エージェントと同時に
走らせるとstate競合のリスクがあるため、確実に自分だけが触っているタイミングで
実行する(status確認や`-target`で自分のリソースだけに操作を限定するのも有効)。

```bash
cd ~/cargo-ci-gcp-spot-instance
terraform init
terraform plan -out=tfplan   # 何が課金対象として作られるか必ず確認する
terraform apply -auto-approve tfplan
```

### 既知の詰まりポイント: `CPUS_ALL_REGIONS`クォータ

新規プロジェクトはデフォルトで`CPUS-ALL-REGIONS-per-project`クォータが**32**しかない。
`variables.tf`の`machine_type`のデフォルトはこれに収まる`n1-highcpu-32`にしてある
(旧世代N1・高CPU数構成を選んでいる理由は「新しい世代(C2/C3系)よりSpotの
preemption率が低い傾向がある」というユーザー指定の要件による——ただし上の
「ステップ2」の実測が示す通り、これは**リージョン選定とセットでないと効かない**。
世代だけ旧くしてもリージョンが悪ければ普通に短時間でpreemptされる)。

もし64以上のマシンタイプに増やしたい場合、自己申請で増枠できる(2026-07-24に実際に確認済み):

```bash
gcloud alpha quotas preferences create \
  --service=compute.googleapis.com \
  --project=<PROJECT_ID> \
  --quota-id=CPUS-ALL-REGIONS-per-project \
  --preferred-value=<希望値> \
  --email=<連絡先メール> \
  --justification="<理由>" \
  --account=<personal-account>
```

**即時反映ではない**(`reconciling: true`で審査待ちになる)。申請は投げつつ、
待たずに現在のクォータに収まるスペックで作業を進めるのが実用的。

## 4. instanceにSSHして依存ツールを入れる(初回のみ)

外部IPは付けていないので、必ず`--tunnel-through-iap`を使う。

```bash
gcloud compute ssh <INSTANCE_NAME> --zone=<ZONE> \
  --project=<PROJECT_ID> --tunnel-through-iap --account=<personal-account> \
  --command='...'
```

起動直後は`startup-script.sh`(apt依存パッケージのインストール・永続ディスクのマウント)が
終わるまで数分かかる。`/tmp/cargo-ci-ready`の有無で完了を判定できる。

rustup/cargo-mutants・Android SDKの実体はstartup-scriptに焼き込まず、SSH後に手動で入れる
(startup-scriptはrootで動くため、ログインユーザーのホームに依存するツールを
焼き込むとユーザー名の食い違いで事故りやすい)。**インストール先は必ず永続ディスク側
(`$CARGO_HOME`/`$RUSTUP_HOME`/`$ANDROID_HOME`/`$GRADLE_USER_HOME`、いずれも
`/etc/environment`にstartup-scriptが設定済み)にする**。これで次回instanceを
作り直してもtoolchain・依存クレート・Android SDKコンポーネント・Gradleの依存
キャッシュが再利用される(Android SDK/NDKは数GB単位で重いのでこれの効果が大きい)。

### rust-core / isekai-ssh 向け(Rustツールチェーン)

```bash
# rustupは--default-toolchain noneだとcargo installがtoolchain未指定で失敗するので
# 明示的にdefaultを設定してから使う
curl --proto "=https" --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --default-toolchain none --no-modify-path
source /etc/environment
export PATH="$CARGO_HOME/bin:$PATH"
rustup default stable
cargo install cargo-mutants --locked   # 必要なツールに応じて変える(cargo-nextest等も同様)
# Windows向けクロスビルド(isekai-ssh)が要るなら:
rustup target add x86_64-pc-windows-gnu
```

### 重要: `isekai-terminal-core`をビルドする前に、まずmusl版`isekai-pipe`を作る

**`isekai-terminal-core`(`cargo test -p isekai-terminal-core`はもちろん、
`cargo mutants -p isekai-terminal-core`も含む)は、`isekai-pipe`のmusl静的バイナリ
(x86_64/aarch64)を`include_bytes!`でソースに埋め込む設計になっている。この埋め込み元
バイナリが存在しないと、**ビルド自体がエラーで失敗する**(cargo-mutantsなら
「baseline(無変異)ですら失敗した」扱いで即中断する):

```
error: couldn't read `.../target/aarch64-unknown-linux-musl/release/isekai-pipe`: No such file or directory
error: couldn't read `.../target/x86_64-unknown-linux-musl/release/isekai-pipe`: No such file or directory
```

**このスキルを初めて使う実例で実際に踏んだ(2026-07-24)。** ローカル開発機では
過去のビルドで`target/`に残っていたため気づきにくいが、GCPの新規instanceは
`target/`が空なので必ず踏む。`rust-core/scripts/build-isekai-pipe-musl.sh`を
**先に**実行して埋め込み用バイナリを作っておく:

zig自体はstartup-scriptが`/mnt/cargo-cache/zig`に展開済み(`$PATH`にも入っている)なので、
ここではrustup target・cargo-zigbuildだけ入れておく(**実際に`build-isekai-pipe-musl.sh`を
実行するのはステップ5でリポジトリを転送し`rust-core/target`のシンボリックリンクを
作った後**——スクリプト自体はリポジトリの中身なので、まだ転送前のこの時点では存在しない):

```bash
source /etc/environment
export PATH="$CARGO_HOME/bin:$PATH"
rustup target add x86_64-unknown-linux-musl aarch64-unknown-linux-musl
cargo install cargo-zigbuild --locked
```

ステップ5の完了後、改めてSSHして以下を実行する:

```bash
source /etc/environment
export PATH="$CARGO_HOME/bin:$PATH"
cd ~/work/rust-core
./scripts/build-isekai-pipe-musl.sh   # target/{x86_64,aarch64}-unknown-linux-musl/release/isekai-pipe を生成
```

出力先は`rust-core/target`(シンボリックリンク経由で永続ディスク側の実体)なので、
**一度作れば次回instanceを作り直しても再利用される**(ソースを変更してisekai-pipe
自体を再ビルドしない限り不要)。

### android/ 向け(Gradle/Kotlin、Rust NDKクロスビルド込み)

Android SDK cmdline-toolsを`$ANDROID_HOME`(永続ディスク側)に展開し、
`compileSdk`/`targetSdk`(`android/build.gradle.kts`参照、変更されていたら都度合わせる)
とNDKを`sdkmanager`で入れる:

```bash
source /etc/environment
COMPILE_SDK=36   # android/build.gradle.ktsのcompileSdkと合わせる
curl -sL https://dl.google.com/android/repository/commandlinetools-linux-11076708_latest.zip -o /tmp/cmdline-tools.zip
mkdir -p "$ANDROID_HOME/cmdline-tools"
unzip -q /tmp/cmdline-tools.zip -d "$ANDROID_HOME/cmdline-tools"
mv "$ANDROID_HOME/cmdline-tools/cmdline-tools" "$ANDROID_HOME/cmdline-tools/latest"
rm /tmp/cmdline-tools.zip

SDKMANAGER="$ANDROID_HOME/cmdline-tools/latest/bin/sdkmanager"
yes | "$SDKMANAGER" --licenses >/dev/null
"$SDKMANAGER" "platform-tools" "platforms;android-$COMPILE_SDK"
# build-toolsは最新版を入れる(AGPが要求するバージョンとズレたらエラーメッセージに従い個別追加)
LATEST_BUILD_TOOLS=$("$SDKMANAGER" --list 2>/dev/null | grep -oE '^  build-tools;[0-9.]+' | sort -V | tail -1 | xargs)
"$SDKMANAGER" "$LATEST_BUILD_TOOLS"
# NDK: rust-core/scripts/ndk-common.sh は $ANDROID_HOME/ndk 配下の最新版を自動選択するので
# バージョンは厳密に固定しなくてよい
LATEST_NDK=$("$SDKMANAGER" --list 2>/dev/null | grep -oE '^  ndk;[0-9.]+' | sort -V | tail -1 | xargs)
"$SDKMANAGER" "$LATEST_NDK"
```

`android/gradle/wrapper/gradle-wrapper.properties`のGradle本体は`./gradlew`が自動DLする
(`$GRADLE_USER_HOME`配下にキャッシュされる)ので別途インストール不要。JDKは
startup-scriptで`openjdk-17-jdk-headless`を入れ済み(`android/build.gradle.kts`の
`sourceCompatibility = JavaVersion.VERSION_17`に対応)。

## 4.5. (推奨)IAP tunnelが遅い/不安定なら、Tailscale経由に切り替える

**2026-07-24に実際に踏んだ問題**: `gcloud compute scp`でAndroid SDK(圧縮後930MB)を
転送したところ、帯域が実測150KB/s程度しか出ず、30分以上経っても107MBのまま進捗が
完全に止まった(`Unexpected error while reconnecting`を繰り返すだけで実データは
流れていなかった)。IAP tunnelは「毎回`--tunnel-through-iap`経由でコマンド1つ実行する」
用途(ステップ4の依存インストール等)には十分だが、**数百MB〜GB級の転送には向かない**。
このボックス(ローカル開発機)が既に個人のTailnetに参加している場合、instanceを
一時的にそのTailnetへ参加させると、SSH/rsyncが直接WireGuard越しになり大幅に速く・
安定する(同じ930MBが数分で完了した実績あり)。

### 前提確認

```bash
tailscale status   # ローカル側が既にtailnetに参加していることを確認
```

参加していなければこのステップはスキップし、素直にIAP tunnelだけで進める。

### a. OAuth clientを作る(ユーザーに管理コンソールで操作してもらう、1回だけ)

`tailscale`のCLI自体にはauth key発行機能が無い(確認済み、`tailscale --help`に
それらしいサブコマンドは無い)。API経由で発行するには、まず管理コンソール
(https://login.tailscale.com/admin/settings/oauth )でOAuth clientを1つ作ってもらう
必要がある。**ユーザーに以下を依頼する**:

- Scope: `Auth Keys` の Write権限
- **Tags**: 作成時に必ず1つ選ばせる欄がある(例: `tag:ci-ephemeral`)。
  **この選択は作成後に編集できない**(admin consoleにEdit機能が無い、Revokeして
  作り直すしかない)。事前にどのタグにするか決めてから作らせること。

発行された Client ID / Client Secret(`tskey-client-...`、表示は一度きり)を
共有してもらったら、gitで管理されない場所に保存する
(例: `~/cargo-ci-gcp-spot-instance/.tailscale-oauth.env`、`chmod 600`、
念のため`.gitignore`にも追記)。

### b. ACLに、そのタグを定義し、SSHアクセスを許可する

ユーザーに現在のACL(https://login.tailscale.com/admin/acls/file )の中身を
貼ってもらい、以下2箇所を確認・追記してもらう。

```json
"tagOwners": {"tag:ci-ephemeral": ["autogroup:admin"]},
```

**さらに重要**: 既定の`"ssh"`ブロックは`"dst": ["autogroup:self"]`(=自分が
所有するデバイスへのSSHのみ許可)なので、tag付きデバイスは対象外になり
`tailscale up --ssh`後に素のOpenSSHすら`tailnet policy does not permit you to SSH
to this node`で弾かれる(2026-07-24に実際に踏んだ)。`"ssh"`配列にタグ向けの
許可を追加してもらう:

```json
{
    "action": "accept",
    "src":    ["autogroup:member"],
    "dst":    ["tag:ci-ephemeral"],
    "users":  ["autogroup:nonroot", "root"],
},
```

(`"action": "check"`だとブラウザでの再認証プロンプトが挟まりCLI越しの自動化に
向かないため`"accept"`にする。)

### c. APIでephemeralなauth keyを発行する

OAuth clientの権限確認だけならACL読み取りAPIも叩けてしまいそうに見えるが、
Auth Keys write専用スコープでは`/acl`の読み取り権限すら無い
(`calling actor does not have enough permissions`になる、想定通り)。
直接auth key発行を試すのが早い:

```bash
set -a; source ~/cargo-ci-gcp-spot-instance/.tailscale-oauth.env; set +a

TOKEN=$(curl -s -X POST https://api.tailscale.com/api/v2/oauth/token \
  -d "client_id=$TAILSCALE_OAUTH_CLIENT_ID" \
  -d "client_secret=$TAILSCALE_OAUTH_CLIENT_SECRET" \
  | python3 -c "import json,sys; print(json.load(sys.stdin)['access_token'])")

curl -s -X POST "https://api.tailscale.com/api/v2/tailnet/-/keys" \
  -H "Authorization: Bearer $TOKEN" -H "Content-Type: application/json" \
  -d '{
    "capabilities": {"devices": {"create": {
      "reusable": false, "ephemeral": true, "preauthorized": true,
      "tags": ["tag:ci-ephemeral"]
    }}},
    "expirySeconds": 3600,
    "description": "<INSTANCE_NAME> spot instance join"
  }' > /tmp/authkey-resp.json
python3 -c "import json; print('has_key:', 'key' in json.load(open('/tmp/authkey-resp.json')))"
```

`"requested tags [...] are invalid or not permitted"`が返ったら、a/bのどちらかが
未完了(ACLの`tagOwners`未追加、またはOAuth client自体にそのタグの権限が付いて
いない)。後者は編集不可なのでRevokeして作り直すしかない(上記a参照)。

### d. instanceをtailnetに参加させる

**生のauth keyをBashコマンドの文字列に直接打ち込まない**(後述「秘密情報の
扱い」参照)。ローカルの一時ファイルに取り出し、IAP tunnel経由でscpしてから
リモート側でファイルを読ませて`shred`する:

```bash
python3 -c "import json; open('/tmp/authkey.txt','w').write(json.load(open('/tmp/authkey-resp.json'))['key'])"
chmod 600 /tmp/authkey.txt

gcloud compute scp /tmp/authkey.txt <INSTANCE_NAME>:/tmp/authkey.txt \
  --zone=<ZONE> --project=<PROJECT_ID> --tunnel-through-iap --account=<personal-account>

gcloud compute ssh <INSTANCE_NAME> --zone=<ZONE> --project=<PROJECT_ID> \
  --tunnel-through-iap --account=<personal-account> --command='
set -e
curl -fsSL https://tailscale.com/install.sh | sh > /tmp/ts-install.log 2>&1
sudo tailscale up --authkey="$(cat /tmp/authkey.txt)" --ssh --hostname=<INSTANCE_NAME> --accept-dns=false
shred -u /tmp/authkey.txt 2>/dev/null || rm -f /tmp/authkey.txt
tailscale ip -4
'
shred -u /tmp/authkey.txt /tmp/authkey-resp.json 2>/dev/null || rm -f /tmp/authkey.txt /tmp/authkey-resp.json
```

出力されたTailscale IP(`100.x.x.x`)を控える。ローカル側の`tailscale status`にも
`<INSTANCE_NAME>`が(tagged devicesとして、ユーザー名列は空で)出るようになる。

### e. SSH configにエイリアスを足し、以後はplain ssh/rsyncを使う

```bash
cat >> ~/.ssh/config <<EOF

Host <INSTANCE_NAME>
    HostName <TAILSCALE_IP>
    User $(whoami)
    IdentityFile ~/.ssh/google_compute_engine
    StrictHostKeyChecking no
EOF
ssh <INSTANCE_NAME> 'echo OK'   # sshdの鍵はgcloud compute sshで既に登録済みのものがそのまま使える
```

これ以降、ステップ5〜8の`gcloud compute scp .../--tunnel-through-iap`は
`rsync -avz --progress <ローカルパス> <INSTANCE_NAME>:<リモートパス>`に、
`gcloud compute ssh ... --command='...'`は`ssh <INSTANCE_NAME> '...'`に
置き換えてよい(体感で大幅に速く、大容量転送でも切れない)。rsyncは差分転送
なので、同じディレクトリを2回目以降syncする時(リポジトリの更新等)は
初回よりさらに速い。ローカル・リモート双方に`rsync`が入っていなければ
先に入れる(ローカルはsudoが使えないサンドボックスでは`brew install rsync`、
リモートはDebianなら`sudo apt-get install -y rsync`)。

### f. 秘密情報(auth key・OAuth secret)の扱いに関する注意

- auth key・client secretは**Bashコマンドの文字列に生の値を直接タイプしない**
  (`export FOO="$(cat file)"`のように、コマンド自体には変数参照だけを書き、
  値はファイル経由で受け渡す)。理由: Claude Codeのセッショントランスクリプト
  (`~/.claude/projects/.../*.jsonl`)は自分が打ったコマンド文字列をそのまま
  記録するため、生の値を直接書くとログに残ってしまう。
- ユーザーがチャットに直接貼ってきた場合、それ自体は表示済みのメッセージなので
  遡って消せないが、`~/.claude/history.jsonl`・該当セッションの`*.jsonl`・
  `~/.claude/file-history/<session-id>/`配下のスナップショットには同じ文字列が
  複数箇所残っている可能性が高い。ユーザーから削除を頼まれたら、対象文字列を
  `grep -rl`で洗い出し、`sed -i`で`[REDACTED_...]`に置換した後、必ず
  ①各jsonlの全行が引き続き valid JSON であることをPythonで検証し、
  ②**確認用の`grep`コマンド自体にその生の文字列を直接書かない**(シェル変数
  経由でのみ参照する。直接書くと確認のたびに新しいログ行として再混入する、
  実際に2026-07-24にこれで一度やり直しになった)。
- 使い終わったauth key・一時ファイル(`authkey.txt`等)は`shred -u`(無ければ
  `rm -f`)で消す。ephemeral keyはinstance切断/削除で自動的にtailnetから
  消えるので、ステップ9の後片付けと一緒に自然に片付く。

## 4.6 Windows Server Spot VMを使う場合の既知の落とし穴(GCEゲストエージェントの不具合)

Windows固有API(WASAPI等)向けにWindows Server Core(`windows-cloud/windows-2022-core`)の
Spot VMを使う場合(rust-nicola・1on1-recorderで実績あり)、GCEのWindowsゲストエージェントが
ドキュメント通りに動かないケースが複数観測されている。**metadata `enable-windows-ssh=TRUE`
だけに頼ると、起動スクリプトが完走してもSSHでログインできない状態になり得る**(2026-07-24、
1on1-recorderのセットアップで実際に以下3点全てを踏んだ)。新規にWindows Server Spot VMを
作る際は、後から1つずつ踏んで直すより、最初から起動スクリプトに以下を全部含めておく方が早い。

### a. OpenSSHサーバが自動導入されないことがある

`enable-windows-ssh=TRUE`だけではsshdサービスが入らないケースがある(ゲストエージェントの
該当コンポーネントが動いていない様子)。起動スクリプトで明示的に`Get-WindowsCapability`/
`Add-WindowsCapability`・`Start-Service sshd`・ファイアウォール開放を自前で行っておく
(既にゲストエージェント側で入っていれば冪等にno-opで返るので無害):

```powershell
if (-not (Get-Service sshd -ErrorAction SilentlyContinue)) {
    $sshCapability = Get-WindowsCapability -Online | Where-Object { $_.Name -like "OpenSSH.Server*" } | Select-Object -First 1
    if ($sshCapability -and $sshCapability.State -ne "Installed") {
        Add-WindowsCapability -Online -Name $sshCapability.Name
    }
}
Set-Service -Name sshd -StartupType Automatic -ErrorAction SilentlyContinue
Start-Service sshd -ErrorAction SilentlyContinue
if (-not (Get-NetFirewallRule -Name "OpenSSH-Server-In-TCP" -ErrorAction SilentlyContinue)) {
    New-NetFirewallRule -Name "OpenSSH-Server-In-TCP" -DisplayName "OpenSSH Server (sshd)" -Enabled True -Direction Inbound -Protocol TCP -Action Allow -LocalPort 22 -ErrorAction SilentlyContinue
}
```

### b. sshdが動いていても、公開鍵がadministrators_authorized_keysに書き込まれないことがある

sshdが起動していてもSSH接続すると`Permission denied (publickey,password,keyboard-interactive)`
で弾かれることがある(host key交換自体は成功するので、一見TCP到達性の問題に見えて紛らわしい)。
原因は同じゲストエージェントの不具合で、project/instance metadataの`ssh-keys`属性(Linuxと
同じ`username:ssh-rsa ... comment`形式)から`C:\ProgramData\ssh\administrators_authorized_keys`
への同期が行われていないこと。これも起動スクリプトで明示的に行う。**administrators_authorized_keys
はOpenSSH側で「Administrators/SYSTEM以外書き込み不可」なACLを要求する**(緩いACLだとsshdが
認証時にこのファイルを黙って無視する)ので、`icacls`で継承を切ってから明示的に権限を締め直す
必要がある:

```powershell
function Get-GceMetadata($path) {
    try { Invoke-RestMethod -Headers @{ "Metadata-Flavor" = "Google" } -Uri "http://metadata.google.internal/computeMetadata/v1/$path`?alt=text" -ErrorAction Stop }
    catch { $null }
}
$authorizedKeysPath = "C:\ProgramData\ssh\administrators_authorized_keys"
$instanceKeys = Get-GceMetadata "instance/attributes/ssh-keys"
$projectKeys  = Get-GceMetadata "project/attributes/ssh-keys"
$publicKeys = @($instanceKeys, $projectKeys) | Where-Object { $_ } | ForEach-Object { $_ -split "`n" } |
    Where-Object { $_.Trim() -ne "" } | ForEach-Object { ($_ -split ":", 2)[-1].Trim() } |
    Where-Object { $_ -match '^ssh-' } | Sort-Object -Unique

if ($publicKeys) {
    New-Item -ItemType Directory -Force -Path (Split-Path $authorizedKeysPath) | Out-Null
    $existing = if (Test-Path $authorizedKeysPath) { Get-Content $authorizedKeysPath } else { @() }
    Set-Content -Path $authorizedKeysPath -Value (@($existing + $publicKeys) | Where-Object { $_.Trim() -ne "" } | Sort-Object -Unique) -Encoding ASCII
    icacls $authorizedKeysPath /inheritance:r | Out-Null
    icacls $authorizedKeysPath /grant "SYSTEM:F" | Out-Null
    icacls $authorizedKeysPath /grant "Administrators:F" | Out-Null
}
```

### c. `Register-ScheduledTask`のRepetitionDurationに`[TimeSpan]::MaxValue`を使わない

idle自動シャットダウン用のScheduled Task登録で、"実質無期限に繰り返す"つもりで
`RepetitionDuration ([TimeSpan]::MaxValue)`を指定すると、Task SchedulerのXMLが受け付けない
範囲外のISO8601 duration(`P99999999DT23H59M59S`)になり、`Register-ScheduledTask`が
「The task XML contains a value which is incorrectly formatted or out of range」で例外を
投げる。`$ErrorActionPreference = "Stop"`の起動スクリプト内でこれが起きると、**この後に
ある`C:\cargo-ci-ready`マーカーファイルの作成まで到達せずスクリプト全体が異常終了する**
(=いつまで待ってもインスタンスが「準備完了」と判定できない、という診断しにくい壊れ方を
する)。実質無期限で十分な有限値(例: `New-TimeSpan -Days 3650`、約10年)を使うこと:

```powershell
$trigger = New-ScheduledTaskTrigger -Once -At (Get-Date) -RepetitionInterval (New-TimeSpan -Minutes 5) -RepetitionDuration (New-TimeSpan -Days 3650)
```

a・b・cはいずれも「起動スクリプトが完走したように見えるのにSSHできない」「`C:\cargo-ci-ready`
がいつまでも現れない」という診断しにくい壊れ方をする点で共通している。起動が怪しいと感じたら
`gcloud compute ssh ... --command='Get-Content C:\cargo-ci-startup-transcript.log -Tail 50'`
(`Start-Transcript`で起動スクリプト全体をログしておくこと)でまず起動スクリプト自体が
最後まで実行されたかを確認するのが早い。

## 5. リポジトリのソースを転送する

このリポジトリは複数エージェントが同時にmainや各worktreeへコミットする運用なので、
`git push`して他のブランチ状態を巻き込むより、**手元の作業ツリーをそのままtarで固めて
scpする**方が安全(未コミットの調査用変更もそのまま持っていける)。作業前に必ず
`git worktree list`/`git log --oneline -5`で他プロセスの並行作業の有無を確認してから
対象を決めること。

対象は基本的に**リポジトリ全体**にする(`rust-core`だけでなく`android/`・`ios/`の
ソースも含む——ビルドしない部分が混ざっていても転送コスト上は問題にならない、
下記の通りビルド成果物さえ除けば全体で数十MB程度)。iOSだけは後述の通りこのインスタンス
では**ビルドできない**が、ソース自体を除外する必要はない(単に使わないだけ)。

```bash
tar -C /home/cuzic/isekai-terminal -czf /tmp/repo-src.tar.gz \
  --exclude=.git \
  --exclude=rust-core/target \
  --exclude=rust-core/mutants.out \
  --exclude=rust-core/mutants.out.old \
  --exclude=rust-core/noq-multipath-spike/target \
  --exclude=android/build \
  --exclude=android/app/build \
  --exclude=android/.gradle \
  --exclude=ios/Frameworks \
  --exclude=ios/.build \
  --exclude=.claude/worktrees \
  rust-core android ios gradle gradlew gradlew.bat build.gradle.kts settings.gradle.kts

gcloud compute scp /tmp/repo-src.tar.gz <INSTANCE_NAME>:/mnt/cargo-cache/repo-src.tar.gz \
  --zone=<ZONE> --project=<PROJECT_ID> --tunnel-through-iap --account=<personal-account>

gcloud compute ssh <INSTANCE_NAME> --zone=<ZONE> --project=<PROJECT_ID> \
  --tunnel-through-iap --account=<personal-account> \
  --command='
mkdir -p ~/work
tar -C ~/work -xzf /mnt/cargo-cache/repo-src.tar.gz
rm -rf ~/work/rust-core/target
ln -s /mnt/cargo-cache/target ~/work/rust-core/target
'
```

`rust-core`単体(`target/`除く)は数MB、`android/src`は数十MB程度で、`.git`(100MB超)や
`android/build`・`ios/Frameworks`のような生成物を除けばリポジトリ全体でも転送はすぐ終わる。
rust-coreだけで足りるタスク(cargo-mutants等)なら、当然`rust-core`だけを対象にしてよい。

**`rust-core/target`をシンボリックリンクにする理由(重要)**: `CARGO_TARGET_DIR`
環境変数で永続ディスクへリダイレクトする方式は**採らない**。`isekai-terminal-core`は
`include_bytes!(concat!(env!("CARGO_MANIFEST_DIR"), "/target/x86_64-unknown-linux-musl/release/isekai-pipe"))`
のように`target/`への相対パスをソースにハードコードしており、これは`CARGO_TARGET_DIR`を
無視する(2026-07-24に実際にこれでbaseline buildが失敗し、原因特定に手間取った)。
シンボリックリンクなら`cargo`からもハードコードされた`include_bytes!`パスからも
同じ実体(`/mnt/cargo-cache/target`)に見えるので両方解決する。

### iOSはこのスキルの対象外

GCP Compute EngineはmacOS VMを提供していない(Apple のライセンス上、AWS EC2 Mac
インスタンスのような特定パートナー以外では一般的に不可)。iOS版(`ios/`、Xcode/Swift
ビルド)はこのスキルでは扱えない。iOS向けの重いビルド検証は引き続き既存の
GitHub Actions `macos-26`ランナー(`.github/workflows/ios-*.yml`)を使う。

## 6. 重いジョブをリモート側でdetach起動する

**`gcloud compute ssh --command`をこのBashツールの`run_in_background`で回しても、
このセッションのターン境界(コンパクション/再開)でジョブごとkillされることが
実際に複数回起きた**(2026-07-24)。ローカルの`run_in_background`はSSH接続プロセスを
管理しているだけで、リモート側のプロセスとは独立していない。

対策: `nohup ... & disown`で**リモート側**にジョブをdetachし、SSHセッション自体が
切れても実行が続くようにする。

rust-core向けの例(cargo-mutants):

```bash
gcloud compute ssh <INSTANCE_NAME> --zone=<ZONE> --project=<PROJECT_ID> \
  --tunnel-through-iap --account=<personal-account> --command='
source /etc/environment
export PATH="$CARGO_HOME/bin:$PATH"
cd ~/work/rust-core
nohup cargo mutants -p isekai-terminal-core -f "orchestrator.rs" --jobs 8 --copy-target true \
  > ~/job.log 2>&1 < /dev/null &
disown
echo "started pid=$!"
'
```

`--copy-target true`は`isekai-terminal-core`(musl静的バイナリの`include_bytes!`埋め込みが
ある)には必須(下の「落とし穴」参照)。`--jobs`はcargo-mutants自身が8超で警告を出すのに
合わせて8にしてある(ブートディスクに`--jobs`回数分`target/`がコピーされるため、
上げすぎない方が安全)。

android/向けの例(Gradle):

```bash
gcloud compute ssh <INSTANCE_NAME> --zone=<ZONE> --project=<PROJECT_ID> \
  --tunnel-through-iap --account=<personal-account> --command='
source /etc/environment
cd ~/work/android
nohup ./gradlew testDebugUnitTest assembleDebug --no-daemon \
  > ~/job.log 2>&1 < /dev/null &
disown
echo "started pid=$!"
'
```

`installDebug`のような実機/エミュレータが要るタスクはこのインスタンスでは動かせない
(このスキルの対象は「ビルド/ユニットテストが重い」問題であって、実機動作確認はスコープ外
——実機確認は引き続き`android-adb-remote`スキル等を使う)。`--no-daemon`はSpot
preemptionで途中終了した際にGradleデーモンが変な状態で残るのを避けるため推奨。

ローカルからSSHで都度ログを覗く。長時間かかる想定なら、20〜30分おきの間隔で
起こしてもらうようスケジューリングして確認する程度で十分(頻繁なpollingは無駄)。

```bash
gcloud compute ssh <INSTANCE_NAME> --zone=<ZONE> --project=<PROJECT_ID> \
  --tunnel-through-iap --account=<personal-account> --command='tail -50 ~/job.log'
```

### SSHが繋がらない/ジョブが中断していたら、まずpreemptionを疑う

ステップ2でpreemption率の低いリージョンを選んでいても、Spotである以上ゼロにはならない
(2026-07-24、実際にステップ2を経ずに選んだ`us-central1-a`で26分後に発生した実例あり)。
SSH接続が`Failed to connect to port 22`で失敗する、または実行中のジョブの標準出力が
突然途切れているように見えたら、まずinstanceの状態を確認する:

```bash
gcloud compute instances describe <INSTANCE_NAME> --zone=<ZONE> --project=<PROJECT_ID> \
  --account=<personal-account> --format="value(status,scheduling.provisioningModel)"

# 直近のpreemptionイベントの有無を確認
gcloud compute operations list --project=<PROJECT_ID> --account=<personal-account> \
  --filter="targetLink~<INSTANCE_NAME>" --format="table(name,operationType,status,insertTime)"
```

`status`が`TERMINATED`で、operations一覧に`compute.instances.preempted`が出ていれば
preemption確定。`instance_termination_action = "STOP"`にしてあるので**ディスクは残っている**
(`DELETE`にしていたら消えていたので、この設定が肝)。再起動して再開する:

```bash
gcloud compute instances start <INSTANCE_NAME> --zone=<ZONE> --project=<PROJECT_ID> \
  --account=<personal-account>
# SSHが通るようになるまで数十秒〜数分ポーリングしてから、ジョブを再開する
```

ジョブの再開方法はツール依存(例: `cargo mutants`なら`--iterate`で既に`caught`済みの
ミュータントをスキップして再開できる、`cargo test`単体ならそのまま再実行すればよい)。
**冪等に再開できないジョブをこの上で走らせる場合は、事前に途中結果を定期的に
ローカルへ回収するか、チェックポイント機構を自分で用意しておくこと**
(preemption前提のインフラである以上、"1回で必ず最後まで通る"ことに依存した設計は避ける)。

## 8. 結果を回収する

```bash
gcloud compute scp <INSTANCE_NAME>:~/work/rust-core/mutants.out/missed.txt /tmp/ \
  --zone=<ZONE> --project=<PROJECT_ID> --tunnel-through-iap --account=<personal-account>
# 必要なファイルを同様にscpで持ち帰る
```

## 9. (オプション)GitHub Actions self-hosted runnerとして登録して使う

`android-ci-deploy`スキル(`build-android.yml`)から`runner_type: self-hosted-gcp-spot`を
選んで実行したい場合、事前にこのinstanceをGitHub Actionsのself-hosted runnerとして
登録しておく必要がある。**ephemeral(1ジョブ限りで自動的に登録解除される)runnerとして
登録する**——常駐runnerにしない(公開リポジトリのself-hosted runnerを常駐させると、
外部からのPRトリガーで悪用され得る攻撃面になるため。今回は`workflow_dispatch`専用の
ワークフローからしか使わない設計だが、それでも「使い終わったら消える」原則は保つ)。

### a. instanceを起動し、登録トークンを取得する

登録トークンは1時間だけ有効・1回登録すると失効する。**Bashコマンドの文字列に
生の値を直接タイプしない**(ステップ4.5「秘密情報の扱い」と同じ理由)。

```bash
gcloud compute instances start <INSTANCE_NAME> --zone=<ZONE> --project=<PROJECT_ID> \
  --account=<personal-account> 2>&1 | grep -v "already running" || true

gh api -X POST repos/<owner>/<repo>/actions/runners/registration-token --jq .token \
  > /tmp/gha-runner-token.txt
chmod 600 /tmp/gha-runner-token.txt
```

### b. runnerエージェントを永続ディスクへ展開する(初回のみ)

エージェント本体(tarball)も`/mnt/cargo-cache`側に置き、instanceを作り直しても
再ダウンロード不要にする。

```bash
scp /tmp/gha-runner-token.txt <INSTANCE_NAME>:/tmp/gha-runner-token.txt
ssh <INSTANCE_NAME> '
set -e
mkdir -p /mnt/cargo-cache/actions-runner
cd /mnt/cargo-cache/actions-runner
if [ ! -f run.sh ]; then
  RUNNER_VERSION=$(curl -s https://api.github.com/repos/actions/runner/releases/latest | python3 -c "import json,sys; print(json.load(sys.stdin)[\"tag_name\"].lstrip(\"v\"))")
  curl -o actions-runner.tar.gz -L \
    "https://github.com/actions/runner/releases/download/v${RUNNER_VERSION}/actions-runner-linux-x64-${RUNNER_VERSION}.tar.gz"
  tar xzf actions-runner.tar.gz
  rm actions-runner.tar.gz
fi
'
```

(Tailscale経由でSSH済みの前提。IAP tunnelしか無い場合は`gcloud compute scp`/
`gcloud compute ssh --tunnel-through-iap`に読み替える。)

### c. ephemeral runnerとして登録し、detach起動する

```bash
ssh <INSTANCE_NAME> '
set -e
cd /mnt/cargo-cache/actions-runner
./config.sh --url https://github.com/<owner>/<repo> \
  --token "$(cat /tmp/gha-runner-token.txt)" \
  --labels android-ci --ephemeral --unattended --replace \
  --name spot-android-ci --work _work
shred -u /tmp/gha-runner-token.txt 2>/dev/null || rm -f /tmp/gha-runner-token.txt
nohup ./run.sh > ~/gha-runner.log 2>&1 < /dev/null &
disown
echo "started pid=$!"
'
rm -f /tmp/gha-runner-token.txt
```

`--ephemeral`により、1回ジョブを処理すると`run.sh`が自動的に終了しGitHub側からも
登録解除される(次に使うときはa〜cを繰り返す)。`--replace`は同名runnerが残って
いた場合に上書きする(preemption等で前回のrunnerがクリーンに登録解除されずに
終わった場合の保険)。

### d. ワークフローを実行する

```bash
gh workflow run build-android.yml -f runner_type=self-hosted-gcp-spot
```

runnerがジョブを拾うまで数秒〜数十秒かかることがある(`gh run watch`で待てばよい)。

### 実際にrunnerが上がっているか確認する

```bash
gh api repos/<owner>/<repo>/actions/runners --jq '.runners[] | {name, status, busy}'
```

`status: "online"`にならない場合、`ssh <INSTANCE_NAME> 'tail -50 ~/gha-runner.log'`で
`run.sh`側のログを確認する。

## 10. 後片付け

**instanceは自動削除されない。** 完了後、明示的に:

```bash
cd ~/cargo-ci-gcp-spot-instance
terraform destroy -target=google_compute_instance.builder
```

`google_compute_disk.cargo_cache`は`prevent_destroy`なので上記では消えない
(意図的、次回の再利用のため)。ネットワーク/firewall/API有効化もコストがほぼ
ゼロなので残してよい。ユーザーから「完全に畳んで」と言われた場合のみ、
`terraform/README.md`の「プロジェクトごと完全に畳みたい場合」の手順に従う。

## この手順を作った経緯で踏んだ落とし穴(再発防止用メモ)

- **`cargo mutants --in-place`は`--jobs`と併用不可**(強制的に直列実行になる)。
  専有マシンでは`--in-place`をやめてデフォルトのコピーモード+`--jobs`で並列化する方が
  圧倒的に速い(ローカルの混雑したサンドボックスでは逆にディスク節約のため
  `--in-place`を使わざるを得なかった)。
- **`cargo mutants -- --skip <test>`だけでは`--skip`が`cargo test`に正しく渡らない**
  (`unexpected argument '--skip'`で失敗する)。`cargo mutants`自身の`--`の後に、
  さらに`cargo test`用の`--`をもう一段挟む必要がある:
  `cargo mutants ... -- -- --skip <test名>`。
- `cargo mutants`は`--jobs`が8を超えると「おそらく高すぎる」と警告を出す
  (`--jobs=16`は32コア専有機では実用上問題なかったが、警告の意味は把握しておく)。
- ローカルの混雑したサンドボックスでは、無関係な別プロジェクトのcargo-mutants実行や
  他の`claude`セッションのビルドとCPUを奪い合っていたことが`ps aux`で直接確認できた
  (このリポジトリは複数エージェントが同時稼働する運用のため、これ自体は珍しくない)。
  `uptime`のload averageとコア数(`nproc`)を突き合わせるのが一番早い診断法。
- **旧世代・高CPU数のマシンタイプを選んでも、リージョンが悪ければ普通にpreemptされる**
  (`us-central1-a`の`n1-highcpu-32`が実際に26分でpreempt済み)。ステップ2の
  `gcloud beta compute advice`による実測は省略しないこと。
- **`isekai-terminal-core`は`CARGO_TARGET_DIR`環境変数を無視する**(`include_bytes!`が
  `env!("CARGO_MANIFEST_DIR")`からの相対パスをハードコードしているため)。永続ディスクへの
  キャッシュはシンボリックリンク(`rust-core/target -> /mnt/cargo-cache/target`)で行う。
  この地雷を踏んでbaseline buildが2回連続で失敗し、原因特定にかなり時間を溶かした。
- **cargo-mutantsのデフォルト(コピー)モードは、mutant毎のスクラッチビルドディレクトリに
  既存の`target/`を含めない**(`--copy-target true`を明示しない限り)。`isekai-terminal-core`は
  上記の埋め込みバイナリが無いとビルドできないので、**`--copy-target true`は必須**
  (付けないとbaselineから`couldn't read .../isekai-pipe: No such file or directory`で
  即失敗する)。`--jobs`を上げるほどスクラッチ用に`target/`をコピーする回数も増えるので、
  ブートディスクの空き容量(`variables.tf`の`boot_disk_size_gb`)と要相談。
- **`~/cargo-ci-gcp-spot-instance`は複数エージェントに共有され得るディレクトリ**であり、
  無関係な別プロジェクト(rust-nicola/awase)が既に同じterraform stateを使っていた実例
  がある。触る前に中身を確認し、他エージェントのリソースを消さないこと(ステップ3参照)。
- **`/mnt/cargo-cache/work`(リポジトリの展開先)も同様に複数エージェントで共有され得る**
  (2026-07-24、実際に別セッションが同じパスへisekai-terminalを展開済みで、そこへ自分の
  tarを無警戒に展開して`PLAN.md`等を上書きしてしまった実例あり)。**必ず
  `/mnt/cargo-cache/work-<プロジェクト名>-<自分の識別子>`のような専用サブディレクトリに
  展開する**(`rust-core/target`のシンボリックリンク先も同様に
  `/mnt/cargo-cache/target-<自分の識別子>`のように分離する)。展開前に`ls /mnt/cargo-cache/`
  で既存のディレクトリ一覧を確認し、他人のものらしき`work`直下への直接展開は避けること。
- **`source /etc/environment`だけでは、その中の変数は子プロセスにexportされない**
  (2026-07-24に実際に踏んだ: `CARGO_HOME`/`ANDROID_HOME`等を`/etc/environment`に足して
  `source /etc/environment`してから`gradlew`を叩いたが、`ANDROID_HOME`が空文字のまま
  Gradleに渡り`SDK location not found`で失敗した)。`/etc/environment`の行は
  `KEY=value`という**ただの代入**であり、bashの`source`はこれを`export`せずローカル
  変数にするだけなので、そのシェルから起動する子プロセス(`gradlew`が更に起動する
  Gradle daemon等)には伝わらない。回避策は2つ:
  1. `--command='...'`の中で**明示的に`export CARGO_HOME=... export ANDROID_HOME=...`
     と書く**(このSKILL.mdの各コード例は元々このパターンで書かれている——省略して
     `source /etc/environment`だけで済ませようとすると同じ地雷を踏む)。
  2. 確実性を上げたいなら、実行コマンド自体を
     `env CARGO_HOME=... ANDROID_HOME=... ./gradlew ...`のように`env`プレフィックスで
     包む(`nohup ... &`でバックグラウンド化する場合、bashの`export`忘れに気づきにくい
     ため、detach起動するコマンドには特にこちらを推奨)。
  なお`CARGO_HOME`/`RUSTUP_HOME`は未exportでも`~/.cargo`/`~/.rustup`という既定値に
  黙ってフォールバックしてしまう(rustup/cargo側にデフォルトパスがあるため気づきにくい)。
  `ANDROID_HOME`にはそのような既定値が無いため必ずエラーで表面化するが、
  **`CARGO_HOME`/`RUSTUP_HOME`は「動いているように見えて実は永続ディスクを使えていない」
  静かな失敗をする**(2026-07-24に実際に発生: ビルド自体は`~/.cargo`/`~/.rustup`という
  ブートディスク側で成功していた——`du -sh ~/.cargo ~/.rustup`と
  `du -sh /mnt/cargo-cache/{cargo-home,rustup-home}`を突き合わせて発覚)。
  作業後に`du -sh ~/.cargo ~/.rustup ~/.gradle`が無視できないサイズになっていたら、
  この地雷を疑い、`rsync -a ~/.gradle/ /mnt/cargo-cache/gradle-home/`のように
  永続ディスク側へ移送してから、以後は上記1/2のいずれかで確実にexportすること。
