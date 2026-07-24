# cargo-ci-gcp-spot-instance

isekai-terminal の rust-core(`isekai-terminal-core`/`isekai-pipe`/`isekai-ssh`等)や
`android/`(Gradle)が重すぎてこのサンドボックス上でビルド/テストを回すのが遅すぎる
(他エージェントとのCPU奪い合い)ため、専用GCPプロジェクト上のSpot VMでビルド/テスト
(cargo-mutants・Gradleユニットテスト等)を実行するためのterraform構成。

**フル手順(プロジェクト確認・リージョン選定・依存インストール・ジョブ実行・
preemption対応・後片付け)は`.claude/skills/cargo-ci-gcp-spot-instance/SKILL.md`を参照。
このREADMEはterraformディレクトリ自体の簡易リファレンス。**

## 前提(terraformの外で一度だけ手動実行が必要)

プロジェクト自体の作成とbilling accountのlinkはterraformで管理していない
(`terraform destroy`がプロジェクトそのものを巻き込む事故を避けるため)。
まだの場合は先に:

```bash
gcloud projects create <PROJECT_ID> --name="<表示名>" --account=<personal-account>
gcloud billing projects link <PROJECT_ID> --billing-account=<BILLING_ACCOUNT_ID> --account=<personal-account>
```

## 使い方

```bash
terraform init
terraform plan
terraform apply
```

インスタンス起動後:

```bash
$(terraform output -raw ssh_command)
```

でIAP tunnel経由(外部IPなし、22番は全世界非公開)でSSHできる。起動直後は
`startup-script.sh`(apt依存パッケージ・JDK・mingw-w64のインストール、永続ディスクの
マウント)が終わるまで数分待つ(`/tmp/cargo-ci-ready`ファイルの有無で判定可能)。
Rustツールチェーン(rustup)本体・Android SDK/NDK・cargo-mutants等の実ツールは
SSH後に手動でセットアップする(ユーザー差異でstartup-script側に焼き込むと事故りやすい
ため)。手順はSKILL.mdのステップ4参照。

## ビルドキャッシュの永続化

`google_compute_disk.cargo_cache`(300GB、`prevent_destroy`指定)を
instance本体とは別ライフサイクルの永続ディスクとして用意し、`/mnt/cargo-cache`に
マウントしている。`CARGO_HOME`/`CARGO_TARGET_DIR`/`RUSTUP_HOME`/`ANDROID_HOME`/
`ANDROID_SDK_ROOT`/`GRADLE_USER_HOME`をそこに向けてあるので(`/etc/profile.d/cargo-ci-env.sh`
と`/etc/environment`の両方に設定済み、ログインシェル・`ssh host 'cmd'`形式どちらでも
効く)、次回instanceを作り直しても依存クレート・Android SDK/NDKコンポーネント・
Gradleの依存の再ダウンロード/再ビルドをゼロからやり直さずに済む。

## 作業完了後

**インスタンスは自動削除されない。** 作業が終わったら明示的に:

```bash
terraform destroy -target=google_compute_instance.builder
```

`google_compute_disk.cargo_cache`は`prevent_destroy`により上記コマンドでは
消えない(意図的)。次回`terraform apply`し直せば同じキャッシュ付きで
instanceだけ作り直される。ネットワーク/ファイアウォール/API有効化も
次回の再利用のために残してよい(コストはほぼゼロ)。

プロジェクトごと完全に畳みたい場合(=キャッシュディスクも含めて全部消す場合)は、
`main.tf`の`prevent_destroy`を外してから`terraform destroy`、その後
`gcloud projects delete <PROJECT_ID>`を別途実行する。

## スペック選定の理由

`n1-highcpu-32`(旧世代N1・32vCPU)を採用。理由は2つ:
1. 新規プロジェクトのデフォルト`CPUS_ALL_REGIONS`クォータ(32)に収まる
   (`n1-highcpu-64`はクォータ超過で作成に失敗した)。
2. 旧世代の方がSpotのpreemption率が低い傾向がある——**ただしこれはリージョン選定と
   セットでないと効かない**。実際に`us-central1-a`では`n1-highcpu-32`でも26分で
   preemptされた実例がある。region/zoneは`gcloud beta compute advice`で実測してから
   決めること(SKILL.mdのステップ2、`../find-low-preemption-region.sh`参照)。
   `variables.tf`の`region`/`zone`デフォルトは2026-07-24の実測に基づき`us-east4`/
   `us-east4-c`にしてある。
