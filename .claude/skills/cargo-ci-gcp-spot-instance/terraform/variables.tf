variable "project_id" {
  description = "isekai-terminal の重いRustビルド/テスト専用に作成したGCPプロジェクト。billingのlinkはterraformの外でgcloudにより一度だけ実施済みの前提。"
  type        = string
  default     = "isekai-terminal-cargo-ci"
}

variable "region" {
  description = "2026-07-24にn1-highcpu-32でgcloud beta compute advice capacity-historyを実測した結果、us-central1(avg preemption 0.36)より低い us-east4(avg 0.14) を既定にしている。マシンタイプを変えたら SKILL.md の手順2で取り直すこと。"
  type        = string
  default     = "us-east4"
}

variable "zone" {
  description = "上記実測でgcloud beta compute advice capacityが推奨したゾーン。"
  type        = string
  default     = "us-east4-c"
}

variable "machine_type" {
  description = "旧世代(N1)かつ高CPU数構成。新しい世代(C2/C3系)よりSpotのpreemption率が低い傾向があるため選定。新規プロジェクトのデフォルトCPUS_ALL_REGIONSクォータ(32)に収まるようn1-highcpu-32とした(n1-highcpu-64はクォータ超過で作成失敗した)。"
  type        = string
  default     = "n1-highcpu-32"
}

variable "boot_disk_size_gb" {
  type    = number
  default = 100
}

variable "cache_disk_size_gb" {
  description = "cargoのregistry/git・target/・Android SDK/NDK・Gradle依存キャッシュを載せる永続ディスクのサイズ。instanceとはライフサイクル分離(prevent_destroy)。Android SDK/NDKが数GB単位で重いので200→300GBに増量した。"
  type        = number
  default     = 300
}

variable "ssh_source_user_email" {
  description = "IAP経由SSHを許可するGoogleアカウント。空ならIAM側の設定を別途行う前提でfirewallのみ用意する。"
  type        = string
  default     = "tomoya.kaw@gmail.com"
}
