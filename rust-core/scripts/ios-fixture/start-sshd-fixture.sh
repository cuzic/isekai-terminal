#!/usr/bin/env bash
# iOS接続テスト用の使い捨てsshd fixtureを起動する(CI/実機検証共通)。
# 固定ホスト鍵・固定ユーザー鍵を都度生成し、127.0.0.1の高番ポートで待ち受ける。
# PLAN.md「Phase Y」節 Phase 1A-3 参照。パスワードはリポジトリに保存せず、
# 実行のたびに使い捨ての鍵ペアを生成する方式にしている。
#
# 使い方:
#   bash rust-core/scripts/ios-fixture/start-sshd-fixture.sh <fixture_dir> <port>
# 生成物: <fixture_dir>/fixture.json (host/port/user/private_key_path/host_key_fingerprint)
# 停止:
#   bash rust-core/scripts/ios-fixture/stop-sshd-fixture.sh <fixture_dir>
set -euo pipefail

FIXTURE_DIR="${1:?usage: start-sshd-fixture.sh <fixture_dir> <port>}"
PORT="${2:?usage: start-sshd-fixture.sh <fixture_dir> <port>}"

command -v sshd >/dev/null 2>&1 || SSHD_BIN=/usr/sbin/sshd
SSHD_BIN="${SSHD_BIN:-$(command -v sshd)}"
[[ -x "$SSHD_BIN" ]] || { echo "error: sshd binary not found" >&2; exit 1; }

mkdir -p "$FIXTURE_DIR"
cd "$FIXTURE_DIR"

rm -f ssh_host_ed25519_key ssh_host_ed25519_key.pub user_ed25519_key user_ed25519_key.pub \
    authorized_keys sshd_config sshd.pid sshd.log fixture.json

ssh-keygen -t ed25519 -N "" -C "ios-ci-fixture-host" -f ssh_host_ed25519_key -q
ssh-keygen -t ed25519 -N "" -C "ios-ci-fixture-user" -f user_ed25519_key -q
cp user_ed25519_key.pub authorized_keys
chmod 600 ssh_host_ed25519_key authorized_keys user_ed25519_key

cat > sshd_config <<EOF
Port ${PORT}
ListenAddress 127.0.0.1
HostKey ${FIXTURE_DIR}/ssh_host_ed25519_key
PidFile ${FIXTURE_DIR}/sshd.pid
AuthorizedKeysFile ${FIXTURE_DIR}/authorized_keys
PubkeyAuthentication yes
PasswordAuthentication no
ChallengeResponseAuthentication no
KbdInteractiveAuthentication no
UsePAM no
StrictModes no
LogLevel VERBOSE
EOF

"$SSHD_BIN" -f "${FIXTURE_DIR}/sshd_config" -E "${FIXTURE_DIR}/sshd.log"

READY=0
for _ in $(seq 1 20); do
    if (exec 3<>"/dev/tcp/127.0.0.1/${PORT}") 2>/dev/null; then
        exec 3>&-
        READY=1
        break
    fi
    sleep 0.5
done
if [[ "$READY" -ne 1 ]]; then
    echo "error: sshd did not start listening on 127.0.0.1:${PORT} within timeout" >&2
    cat "${FIXTURE_DIR}/sshd.log" >&2 || true
    exit 1
fi

HOST_KEY_FINGERPRINT="$(ssh-keygen -lf ssh_host_ed25519_key.pub | awk '{print $2}')"

cat > fixture.json <<EOF
{
  "host": "127.0.0.1",
  "port": ${PORT},
  "user": "$(whoami)",
  "private_key_path": "${FIXTURE_DIR}/user_ed25519_key",
  "host_key_fingerprint": "${HOST_KEY_FINGERPRINT}"
}
EOF

echo "sshd fixture listening on 127.0.0.1:${PORT} (pid $(cat "${FIXTURE_DIR}/sshd.pid"))"
cat fixture.json
