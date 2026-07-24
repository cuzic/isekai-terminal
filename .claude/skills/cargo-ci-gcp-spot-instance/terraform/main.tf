resource "google_project_service" "compute" {
  project            = var.project_id
  service            = "compute.googleapis.com"
  disable_on_destroy = false
}

resource "google_project_service" "iap" {
  project            = var.project_id
  service            = "iap.googleapis.com"
  disable_on_destroy = false
}

resource "google_compute_network" "ci" {
  project                 = var.project_id
  name                    = "cargo-ci"
  auto_create_subnetworks = false
  depends_on              = [google_project_service.compute]
}

resource "google_compute_subnetwork" "ci" {
  project       = var.project_id
  name          = "cargo-ci"
  network       = google_compute_network.ci.id
  region        = var.region
  ip_cidr_range = "10.20.0.0/24"
}

# 22番を全世界公開せず、Identity-Aware Proxy のSSHトンネル経由のみ許可する。
resource "google_compute_firewall" "allow_iap_ssh" {
  project       = var.project_id
  name          = "allow-iap-ssh"
  network       = google_compute_network.ci.id
  direction     = "INGRESS"
  source_ranges = ["35.235.240.0/20"]

  allow {
    protocol = "tcp"
    ports    = ["22"]
  }
}

# 外部IPを付けないので、apt/cargoのダウンロード用にCloud NATで送信専用の経路を用意する。
resource "google_compute_router" "ci" {
  project = var.project_id
  name    = "cargo-ci-router"
  network = google_compute_network.ci.id
  region  = var.region
}

resource "google_compute_router_nat" "ci" {
  project                            = var.project_id
  name                               = "cargo-ci-nat"
  router                             = google_compute_router.ci.name
  region                             = var.region
  nat_ip_allocate_option             = "AUTO_ONLY"
  source_subnetwork_ip_ranges_to_nat = "ALL_SUBNETWORKS_ALL_IP_RANGES"
}

resource "google_project_iam_member" "iap_tunnel_user" {
  project = var.project_id
  role    = "roles/iap.tunnelResourceAccessor"
  member  = "user:${var.ssh_source_user_email}"

  depends_on = [google_project_service.iap]
}

# cargoのregistry/gitキャッシュとtarget/を載せる永続ディスク。
# instance本体(Spot、使い終わったら terraform destroy -target で消す前提)とは
# ライフサイクルを分離しておき、次回インスタンスを作り直しても
# 依存クレートの再ダウンロード・依存クレートの再ビルドをゼロからやり直さずに済むようにする。
resource "google_compute_disk" "cargo_cache" {
  project = var.project_id
  name    = "cargo-ci-cache"
  zone    = var.zone
  type    = "pd-balanced"
  size    = var.cache_disk_size_gb

  lifecycle {
    prevent_destroy = true
  }
}

resource "google_compute_instance" "builder" {
  project      = var.project_id
  name         = "isekai-terminal-builder"
  machine_type = var.machine_type
  zone         = var.zone

  # preemption済みでもdiskは保持(STOP)。完了後の削除は明示的な terraform destroy で行う。
  scheduling {
    provisioning_model          = "SPOT"
    preemptible                 = true
    automatic_restart           = false
    instance_termination_action = "STOP"
  }

  boot_disk {
    initialize_params {
      image = "debian-cloud/debian-12"
      size  = var.boot_disk_size_gb
      type  = "pd-balanced"
    }
  }

  attached_disk {
    source      = google_compute_disk.cargo_cache.id
    device_name = "cargo-cache"
  }

  network_interface {
    subnetwork = google_compute_subnetwork.ci.id
    # 外部IPは付けない。IAP tunnel経由でのみアクセスする。
  }

  metadata_startup_script = file("${path.module}/startup-script.sh")

  labels = {
    purpose = "isekai-terminal-heavy-build-ci"
  }

  depends_on = [
    google_project_service.compute,
    google_compute_firewall.allow_iap_ssh,
  ]
}

output "instance_name" {
  value = google_compute_instance.builder.name
}

output "zone" {
  value = var.zone
}

output "ssh_command" {
  value = "gcloud compute ssh ${google_compute_instance.builder.name} --zone=${var.zone} --project=${var.project_id} --tunnel-through-iap --account=${var.ssh_source_user_email}"
}
