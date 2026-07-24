#!/usr/bin/env bash
set -euxo pipefail

# rust-core(cargo)向けとandroid/(Gradle)向け、両方のビルド依存をここでまとめて入れる。
# isekai-ssh(rust-coreと独立したworkspace)もrust-core向けの依存だけで足りる。
# iOSはmacOS専用ツールチェーンが必要でGCP Compute Engineでは動かせないので対象外
# (iOSはこれまで通りGitHub Actionsのmacosランナーを使う)。
apt-get update
apt-get install -y \
  build-essential pkg-config libssl-dev git curl unzip clang cmake musl-tools \
  gcc-mingw-w64-x86-64 g++-mingw-w64-x86-64 \
  openjdk-17-jdk-headless

# --- 永続キャッシュディスクのマウント ---
# device_name="cargo-cache" で attach しているので /dev/disk/by-id/google-cargo-cache は安定パス。
DISK=/dev/disk/by-id/google-cargo-cache
MOUNT=/mnt/cargo-cache

for i in $(seq 1 30); do
  [ -e "$DISK" ] && break
  sleep 1
done

if ! blkid "$DISK" >/dev/null 2>&1; then
  # 初回のみ(空ディスク): ext4でフォーマット
  mkfs.ext4 -F "$DISK"
fi

mkdir -p "$MOUNT"
mountpoint -q "$MOUNT" || mount "$DISK" "$MOUNT"
grep -q "$MOUNT" /etc/fstab || echo "$DISK $MOUNT ext4 discard,defaults,nofail 0 2" >> /etc/fstab

mkdir -p "$MOUNT/cargo-home" "$MOUNT/target" "$MOUNT/rustup-home" \
  "$MOUNT/android-sdk" "$MOUNT/gradle-home"
chmod -R 1777 "$MOUNT"

# zig(cargo-zigbuild経由のmuslクロスビルドに必須、isekai-terminal-coreがビルドに
# 埋め込むisekai-pipeのmusl静的バイナリを作るのに要る)。ただの静的バイナリで
# ユーザー固有の状態を持たないのでrootのstartup-scriptで入れてしまってよい。
# `pip install ziglang`はこのリポジトリのフック(Pythonはuv必須ルール)に
# 引っかかるので、公式tarballを直接展開する。
if [ ! -x "$MOUNT/zig/zig" ]; then
  curl -sL https://ziglang.org/download/0.13.0/zig-linux-x86_64-0.13.0.tar.xz -o /tmp/zig.tar.xz
  tar -C /tmp -xf /tmp/zig.tar.xz
  mkdir -p "$MOUNT/zig"
  cp -r /tmp/zig-linux-x86_64-0.13.0/* "$MOUNT/zig/"
  rm -rf /tmp/zig.tar.xz /tmp/zig-linux-x86_64-0.13.0
fi

# cargo/rustup・Android SDK・Gradleのキャッシュを全てこのディスク上に固定する。
# instanceを作り直しても(=このstartup-scriptが再実行されても)このディスクは
# prevent_destroy されているので、依存のDL・ビルド成果物・SDKコンポーネントは
# 再利用され続ける(特にAndroid SDK/NDKのDLは数GB単位で重いので効果が大きい)。
# CARGO_TARGET_DIRは意図的に設定しない: isekai-terminal-coreは
# `include_bytes!(concat!(env!("CARGO_MANIFEST_DIR"), "/target/..."))`で
# isekai-pipeのmusl静的バイナリを埋め込む設計で、このパスはCARGO_TARGET_DIRを
# 無視する(2026-07-24に実際にこれでbaseline buildが失敗する原因になった)。
# 代わりにリポジトリ展開後、`ln -s /mnt/cargo-cache/target <repo>/rust-core/target`
# のようにシンボリックリンクで永続化する(SKILL.mdステップ5参照)。
cat > /etc/profile.d/cargo-ci-env.sh <<'EOF'
export CARGO_HOME=/mnt/cargo-cache/cargo-home
export RUSTUP_HOME=/mnt/cargo-cache/rustup-home
export ANDROID_HOME=/mnt/cargo-cache/android-sdk
export ANDROID_SDK_ROOT=/mnt/cargo-cache/android-sdk
export GRADLE_USER_HOME=/mnt/cargo-cache/gradle-home
export JAVA_HOME=/usr/lib/jvm/java-17-openjdk-amd64
export PATH="$CARGO_HOME/bin:/mnt/cargo-cache/zig:$ANDROID_HOME/cmdline-tools/latest/bin:$ANDROID_HOME/platform-tools:$PATH"
EOF
chmod 644 /etc/profile.d/cargo-ci-env.sh

# /etc/environment はログインシェルでない `ssh host 'cmd'` 形式の実行でも
# PAM経由で読まれるので、profile.d と重複させてこちらにも書く(PATHは
# /etc/environment では展開されないのでprofile.d側だけがPATH担当)。
grep -q CARGO_HOME /etc/environment || cat >> /etc/environment <<'EOF'
CARGO_HOME=/mnt/cargo-cache/cargo-home
RUSTUP_HOME=/mnt/cargo-cache/rustup-home
ANDROID_HOME=/mnt/cargo-cache/android-sdk
ANDROID_SDK_ROOT=/mnt/cargo-cache/android-sdk
GRADLE_USER_HOME=/mnt/cargo-cache/gradle-home
JAVA_HOME=/usr/lib/jvm/java-17-openjdk-amd64
EOF

touch /tmp/cargo-ci-ready
