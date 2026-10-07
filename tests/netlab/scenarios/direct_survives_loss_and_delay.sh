#!/usr/bin/env bash
# ベースライン: 実バイナリ(isekai-pipe serve/connect + 実sshd + 実ssh)を、veth1本で
# 直結した2つのnetwork namespaceの上で動かし、tc netemで注入した
# パケットロス/遅延の下でも1本のSSHセッションでバイト列が壊れずに
# 往復できることを検証する(PLAN.md:1322「物理2ネットワークでの実機検証は
# 未実施」のギャップに対する、CI上での代替)。
#
# スコープ外: NAT、MASQUE relayフォールバック、isekai-sshのbootstrap-over-ssh。
# 共通のスタック構築は ../common.sh(trust storeはserveのハンドシェイクJSONから
# 直接組み立てる)。
#
# 使い方:
#   cargo build -p isekai-pipe --bin isekai-pipe
#   sudo ISEKAI_PIPE_BIN="$PWD/target/debug/isekai-pipe" \
#       tests/netlab/scenarios/direct_survives_loss_and_delay.sh

NETLAB_SCENARIO_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=../common.sh
source "$NETLAB_SCENARIO_DIR/../common.sh"

NETLAB_LOSS="${NETLAB_LOSS:-3%}"
NETLAB_DELAY="${NETLAB_DELAY:-80ms 20ms}"
PAYLOAD_BYTES="${PAYLOAD_BYTES:-2097152}"

echo "== workdir: $WORKDIR =="
echo "== loss=$NETLAB_LOSS delay=$NETLAB_DELAY payload=${PAYLOAD_BYTES}B =="

netlab_up
netlab_apply_netem "$NETLAB_LOSS" "$NETLAB_DELAY"
netlab_stack_up --once

head -c "$PAYLOAD_BYTES" /dev/urandom > "$WORKDIR/payload.bin"
LOCAL_SUM="$(sha256sum "$WORKDIR/payload.bin" | awk '{print $1}')"

set +e
netlab_ssh sha256sum < "$WORKDIR/payload.bin" > "$WORKDIR/remote_sum.txt" 2> "$WORKDIR/ssh.log"
SSH_STATUS=$?
set -e

wait "$SERVE_PID" 2>/dev/null || true
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
