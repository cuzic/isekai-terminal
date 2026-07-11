#!/usr/bin/env bash
# PoC: 実バイナリ(isekai-pipe serve/connect + 実sshd + 実ssh)を、veth1本で
# 直結した2つのnetwork namespaceの上で動かし、tc netemで注入した
# パケットロス/遅延の下でも1本のSSHセッションでバイト列が壊れずに
# 往復できることを検証する(PLAN.md:1322「物理2ネットワークでの実機検証は
# 未実施」のギャップに対する、CI上での代替)。
#
# スコープ外(このPoCではやらない): NAT、MASQUE relayフォールバック
# (relayサーバーのstandaloneバイナリが存在しないため、ISEKAI_PIPE_DESIGN.md
# 参照)、isekai-sshのbootstrap-over-ssh(wrapper_auto_bootstrap_e2e.rsで
# 別途カバー済み・ネットワーク耐性とは無関係)。client側のtrust store
# (known_helpers.toml)は`isekai-pipe serve`が起動時にstdoutへ出す
# ハンドシェイクJSONから直接組み立てる — bootstrap経由の場合に
# isekai-sshが書き込むのと同じ形式(isekai-trust::schema::HelperTrust)。
#
# 前提: root権限(ip netns/veth/tc/sshd)、`ssh`/`sshd`/`ssh-keygen`/`jq`が
# PATH上にあること、ISEKAI_PIPE_BIN に事前ビルド済み`isekai-pipe`バイナリの
# 絶対パスを渡すこと(このスクリプト自身はcargo buildを叩かない —
# rootで実行するとtarget/がroot所有に汚染されるため)。
#
# 使い方:
#   cargo build -p isekai-pipe --bin isekai-pipe
#   sudo ISEKAI_PIPE_BIN="$PWD/target/debug/isekai-pipe" \
#       tests/netlab/scenarios/direct_survives_loss_and_delay.sh

set -euo pipefail

if [ "$(id -u)" -ne 0 ]; then
    echo "must run as root (sudo) — needs ip netns/veth/tc/sshd" >&2
    exit 1
fi

: "${ISEKAI_PIPE_BIN:?set ISEKAI_PIPE_BIN to a prebuilt isekai-pipe binary path}"
if [ ! -x "$ISEKAI_PIPE_BIN" ]; then
    echo "ISEKAI_PIPE_BIN=$ISEKAI_PIPE_BIN is not an executable file" >&2
    exit 1
fi
ISEKAI_PIPE_BIN="$(readlink -f "$ISEKAI_PIPE_BIN")"

for bin in ssh ssh-keygen jq sha256sum tc; do
    command -v "$bin" >/dev/null 2>&1 || { echo "missing required tool on PATH: $bin" >&2; exit 1; }
done
[ -x /usr/sbin/sshd ] || { echo "missing required tool: /usr/sbin/sshd" >&2; exit 1; }

NETLAB_LOSS="${NETLAB_LOSS:-3%}"
NETLAB_DELAY="${NETLAB_DELAY:-80ms 20ms}"
PAYLOAD_BYTES="${PAYLOAD_BYTES:-2097152}"
SSH_LOGIN_USER="${SUDO_USER:-$(whoami)}"

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
NETLAB_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"
# shellcheck source=../topology.sh
source "$NETLAB_DIR/topology.sh"

WORKDIR="$(mktemp -d)"
SERVE_PID=""
SSHD_PID=""
UNLOCKED_LOGIN_USER=""

cleanup() {
    local exit_code=$?
    set +e
    [ -n "$SERVE_PID" ] && kill "$SERVE_PID" 2>/dev/null
    [ -n "$SSHD_PID" ] && kill "$SSHD_PID" 2>/dev/null
    [ -n "$UNLOCKED_LOGIN_USER" ] && passwd -l "$UNLOCKED_LOGIN_USER" >/dev/null 2>&1
    if [ "$exit_code" -ne 0 ]; then
        echo "=== FAILURE: dumping diagnostics ===" >&2
        netlab_diagnostics >&2
        for f in "$WORKDIR"/sshd.log "$WORKDIR"/serve.stdout "$WORKDIR"/serve.stderr "$WORKDIR"/ssh.log; do
            [ -f "$f" ] && { echo "--- $f ---" >&2; cat "$f" >&2; }
        done
        echo "--- dmesg (tail) ---" >&2
        dmesg -T 2>&1 | tail -80 >&2
    fi
    netlab_down
    rm -rf "$WORKDIR"
    exit "$exit_code"
}
trap cleanup EXIT

echo "== workdir: $WORKDIR =="
echo "== loss=$NETLAB_LOSS delay=$NETLAB_DELAY payload=${PAYLOAD_BYTES}B =="

netlab_up
netlab_apply_netem "$NETLAB_LOSS" "$NETLAB_DELAY"

# --- 実sshdをserver ns内、127.0.0.1にだけ立てる(isekai-pipe serveが
# --targetでローカル転送する先。実sshクライアントはQUIC越しにしか
# 到達しないので、veth側アドレスにbindする必要はない)。
ssh-keygen -t ed25519 -N '' -q -f "$WORKDIR/host_key"
ssh-keygen -t ed25519 -N '' -q -f "$WORKDIR/client_key"
cp "$WORKDIR/client_key.pub" "$WORKDIR/authorized_keys"
chmod 600 "$WORKDIR/host_key" "$WORKDIR/client_key"
# sshdはAuthorizedKeysFileを(StrictModes noでもなお)ログインユーザーの
# 権限に落としてから読むため、root所有・700のWORKDIR配下に置くだけでは
# $SSH_LOGIN_USERから読めない。鍵は公開鍵なので世界読み取り可でよい。
chmod 711 "$WORKDIR"
chmod 644 "$WORKDIR/authorized_keys"

cat > "$WORKDIR/sshd_config" <<EOF
Port 2222
ListenAddress 127.0.0.1
HostKey $WORKDIR/host_key
AuthorizedKeysFile $WORKDIR/authorized_keys
PidFile $WORKDIR/sshd.pid
PasswordAuthentication no
KbdInteractiveAuthentication no
PubkeyAuthentication yes
UsePAM no
StrictModes no
LogLevel VERBOSE
EOF

# sshdはPasswordAuthentication no/pubkeyのみでも、ログインユーザーの
# shadowパスワードが"locked"(先頭!、GitHub Actionsのrunnerユーザー等)だと
# "account is locked"でpreauth拒否する。`passwd -u`はそもそもパスワード
# ハッシュが無い(!!)アカウントには"passwordless account"として拒否される
# ことがあるため、使い捨てのランダムパスワードをchpasswdで設定して
# unlockする(PasswordAuthentication noなので実際にログインには使えない)。
# 元がlockedだった場合はcleanupで必ずlockし直す。
if passwd -S "$SSH_LOGIN_USER" 2>/dev/null | awk '{exit ($2 == "L") ? 0 : 1}'; then
    echo "$SSH_LOGIN_USER:$(head -c 32 /dev/urandom | base64)" | chpasswd
    UNLOCKED_LOGIN_USER="$SSH_LOGIN_USER"
fi

mkdir -p /run/sshd
chmod 755 /run/sshd
ip netns exec "$NETLAB_SERVER_NS" /usr/sbin/sshd -f "$WORKDIR/sshd_config" -D -e \
    > "$WORKDIR/sshd.log" 2>&1 &
SSHD_PID=$!

for _ in $(seq 1 50); do
    ip netns exec "$NETLAB_SERVER_NS" bash -c 'echo > /dev/tcp/127.0.0.1/2222' 2>/dev/null && break
    sleep 0.2
done

# --- isekai-pipe serve: server ns内、UDPを直接bind(direct mode)。
ip netns exec "$NETLAB_SERVER_NS" "$ISEKAI_PIPE_BIN" serve \
    --target 127.0.0.1:2222 --bind 0.0.0.0:0 --once --log-level debug \
    > "$WORKDIR/serve.stdout" 2> "$WORKDIR/serve.stderr" &
SERVE_PID=$!

for _ in $(seq 1 50); do
    [ -s "$WORKDIR/serve.stdout" ] && break
    sleep 0.2
done
if [ ! -s "$WORKDIR/serve.stdout" ]; then
    echo "isekai-pipe serve never printed a handshake line" >&2
    exit 1
fi

SESSION_SECRET_B64="$(jq -r '.session_secret' "$WORKDIR/serve.stdout")"
CERT_SHA256="$(jq -r '.peer.server_identity.cert_sha256' "$WORKDIR/serve.stdout")"
QUIC_PORT="$(jq -r '.candidates[0].port' "$WORKDIR/serve.stdout")"

# --- client ns側のtrust store(known_helpers.toml)を、bootstrap経由の
# isekai-ssh initが書くのと同じ形式で直接組み立てる(schema.rs参照)。
CLIENT_HOME="$WORKDIR/client-home"
TRUST_DIR="$CLIENT_HOME/.config/isekai-ssh"
mkdir -p "$TRUST_DIR"
cat > "$TRUST_DIR/known_helpers.toml" <<EOF
[helpers."poc-host:22"]
identity_pubkey = "unused-by-legacy-connect-path"
trusted_helper_sha256 = "$(printf '0%.0s' $(seq 1 64))"
trusted_helper_version = "0.0.0-netlab-poc"
update_policy = "exact-digest-only"
trusted_at = "1970-01-01T00:00:00Z"
last_seen_at = "1970-01-01T00:00:00Z"
cached_relay_addr = "$NETLAB_SERVER_IP:$QUIC_PORT"
cached_cert_sha256 = "$CERT_SHA256"
cached_session_secret = "$SESSION_SECRET_B64"
EOF
chmod 700 "$TRUST_DIR"
chmod 600 "$TRUST_DIR/known_helpers.toml"

# --- 往復させるペイロードを用意し、実sshクライアントでリモートの
# sha256sumへパイプする。ロス/遅延はQUICトランスポート層で吸収される
# はず(壊れていたらsha256が一致しない)。
head -c "$PAYLOAD_BYTES" /dev/urandom > "$WORKDIR/payload.bin"
LOCAL_SUM="$(sha256sum "$WORKDIR/payload.bin" | awk '{print $1}')"

set +e
timeout 60 ip netns exec "$NETLAB_CLIENT_NS" env \
    HOME="$CLIENT_HOME" PATH="$PATH" \
    RUST_LOG=isekai_transport=debug,isekai_pipe=debug \
    ssh -F /dev/null \
        -o IdentityFile="$WORKDIR/client_key" \
        -o IdentitiesOnly=yes \
        -o PreferredAuthentications=publickey \
        -o BatchMode=yes \
        -o StrictHostKeyChecking=no \
        -o UserKnownHostsFile=/dev/null \
        -o ConnectTimeout=30 \
        -o ProxyCommand="$ISEKAI_PIPE_BIN connect --profile poc-host --service ssh --stdio" \
        "$SSH_LOGIN_USER@poc-host" 'sha256sum' \
        < "$WORKDIR/payload.bin" > "$WORKDIR/remote_sum.txt" 2> "$WORKDIR/ssh.log"
SSH_STATUS=$?
set -e

wait "$SERVE_PID" 2>/dev/null
SERVE_PID=""

if [ "$SSH_STATUS" -ne 0 ]; then
    echo "ssh exited $SSH_STATUS" >&2
    exit 1
fi

REMOTE_SUM="$(awk '{print $1}' "$WORKDIR/remote_sum.txt")"
if [ "$LOCAL_SUM" != "$REMOTE_SUM" ]; then
    echo "checksum mismatch: local=$LOCAL_SUM remote=$REMOTE_SUM" >&2
    exit 1
fi

echo "OK: ${PAYLOAD_BYTES}B round-tripped intact over QUIC under loss=$NETLAB_LOSS delay=$NETLAB_DELAY (sha256=$LOCAL_SUM)"
