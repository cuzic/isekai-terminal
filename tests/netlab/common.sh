#!/usr/bin/env bash
# netlabシナリオ共通ライブラリ(sourceして使う)。root権限・ISEKAI_PIPE_BINが前提。
# 各シナリオは `source common.sh` → `netlab_up` → `netlab_stack_up <serve追加引数>`
# → シナリオ固有の故障注入と検証、の順で書く。cleanup(失敗時の診断ダンプ含む)は
# このファイルがtrap EXITで登録する。
#
# 構成: server ns内に実sshd(127.0.0.1:2222)と`isekai-pipe serve`、client nsから
# 実`ssh -o ProxyCommand="isekai-pipe connect ..."`で接続する(実カーネル・実QUIC・実時間)。

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

for bin in ssh ssh-keygen jq sha256sum tc iptables; do
    command -v "$bin" >/dev/null 2>&1 || { echo "missing required tool on PATH: $bin" >&2; exit 1; }
done
[ -x /usr/sbin/sshd ] || { echo "missing required tool: /usr/sbin/sshd" >&2; exit 1; }

NETLAB_LOSS="${NETLAB_LOSS:-3%}"
NETLAB_DELAY="${NETLAB_DELAY:-80ms 20ms}"
PAYLOAD_BYTES="${PAYLOAD_BYTES:-2097152}"
SSH_LOGIN_USER="${SUDO_USER:-$(whoami)}"

NETLAB_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=topology.sh
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


# $@ = `isekai-pipe serve`への追加引数(例: --once、--idle-timeout 60)。
# 呼ぶ前に netlab_up 済みであること。
netlab_stack_up() {
# (heredocの終端を桁0に保つため、関数本体はインデントしない)
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
    --target 127.0.0.1:2222 --bind 0.0.0.0:0 --log-level debug "$@" \
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

# --- client ns側のPersistentProfile(isekai-pipe-core::profile)を、bootstrap経由の
# isekai-ssh initが書くのと同じ形式で直接組み立てる。旧known_helpers.tomlは
# もうどのlive pathからも読まれない(PoC作成後に移行済み)ので、
# <profiles dir>/poc-host%3A22.json に書く(':'は%3Aにエスケープされる)。
CLIENT_HOME="$WORKDIR/client-home"
PROFILES_DIR="$CLIENT_HOME/profiles"
mkdir -p "$PROFILES_DIR"
jq -n \
    --arg cert "$CERT_SHA256" \
    --arg addr "$NETLAB_SERVER_IP:$QUIC_PORT" \
    --arg secret "$SESSION_SECRET_B64" \
    --arg zeros "$(printf '0%.0s' $(seq 1 64))" \
    '{
        schema_version: 2,
        profile: "poc-host:22",
        server_identity: {cert_sha256_hex: $cert},
        service: "ssh",
        relay_policy: "relay-allowed",
        legacy_relay_transport: {helper_addr: $addr, session_secret_b64: $secret},
        identity_pubkey: "unused-by-legacy-connect-path",
        trusted_helper_sha256: $zeros,
        update_policy: "exact-digest-only",
        last_seen_at: "1970-01-01T00:00:00Z"
    }' > "$PROFILES_DIR/poc-host%3A22.json"
chmod 700 "$PROFILES_DIR"
chmod 600 "$PROFILES_DIR/poc-host%3A22.json"

}

# client ns内から実sshでprofile `poc-host`へ接続し、リモートで $1 を実行する。
# stdinはそのまま渡る。$NETLAB_SSH_TIMEOUT秒(既定60)でtimeout。
netlab_ssh() {
    timeout "${NETLAB_SSH_TIMEOUT:-60}" ip netns exec "$NETLAB_CLIENT_NS" env \
        HOME="$CLIENT_HOME" ISEKAI_PIPE_PROFILES_DIR="$PROFILES_DIR" PATH="$PATH" \
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
            "$SSH_LOGIN_USER@poc-host" "$1"
}

# 4096Bずつ、1秒おきに$1回、$WORKDIR/payload.binの先頭から順に標準出力へ流す
# (故障注入の最中もデータが流れ続け、再送・replayが効くことを見るため)。
netlab_paced_feeder() {
    local n="$1" i
    for i in $(seq 0 $((n - 1))); do
        dd if="$WORKDIR/payload.bin" bs=4096 skip="$i" count=1 status=none
        sleep 1
    done
}

# pacedシナリオの共通実行部。$1=ペース回数、$2=故障注入関数名(バックグラウンドで
# 実行される。ssh開始の5秒後に呼ばれる)。完走・sha256一致・QUIC接続が2本以上
# (=実際にresumeした)ことを検証する。$3=最低QUIC接続数(既定2)。
netlab_run_paced_scenario() {
    local n="$1" inject="$2" min_conns="${3:-2}"
    head -c $((n * 4096)) /dev/urandom > "$WORKDIR/payload.bin"
    local local_sum
    local_sum="$(sha256sum "$WORKDIR/payload.bin" | awk '{print $1}')"

    ( sleep 5; echo "== [$(date +%T)] inject: $inject =="; "$inject" ) &
    local inject_pid=$!

    local t0 status
    t0=$(date +%s)
    set +e
    netlab_paced_feeder "$n" | NETLAB_SSH_TIMEOUT="${NETLAB_SSH_TIMEOUT:-150}" netlab_ssh sha256sum \
        > "$WORKDIR/remote_sum.txt" 2> "$WORKDIR/ssh.log"
    status=${PIPESTATUS[1]}
    set -e
    wait "$inject_pid" || { echo "fault injection failed" >&2; exit 1; }
    echo "== ssh finished status=$status after $(( $(date +%s) - t0 ))s =="

    [ "$status" -eq 0 ] || { echo "ssh exited $status" >&2; exit 1; }
    local remote_sum
    remote_sum="$(awk '{print $1}' "$WORKDIR/remote_sum.txt")"
    [ "$local_sum" = "$remote_sum" ] || { echo "checksum mismatch: local=$local_sum remote=$remote_sum" >&2; exit 1; }

    local conns
    conns="$(grep -c 'QUIC connection established' "$WORKDIR/serve.stderr" || true)"
    echo "== serve saw $conns QUIC connection(s) (need >= $min_conns) =="
    if [ "$conns" -lt "$min_conns" ]; then
        echo "session survived without a resume; the fault was not effective" >&2
        exit 1
    fi
    echo "OK: ${n}x4096B round-tripped intact across the fault (sha256=$local_sum, conns=$conns)"
}
