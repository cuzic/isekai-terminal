#!/usr/bin/env bash
# Phase 5 判断ゲート (#71): TCP SSH 往復遅延の計測
#
# Tailscale 経由の TCP SSH が steady-state でどの程度の RTT を持つかを計測し、
# tsshd/QUIC へ移行する価値があるか（接続移行オーバーヘッドの基準値）を判断する。
#
# Usage: ./scripts/measure_latency.sh [host] [port] [user] [iterations]
#   host  : 接続先 (default: 100.100.45.36 = Tailscale 上の Linux サーバー)
#   port  : SSH ポート (default: 22)
#   user  : ログインユーザー (default: cuzic)
#   iters : SSH 往復計測の試行回数 (default: 30)

set -euo pipefail

HOST="${1:-100.100.45.36}"
PORT="${2:-22}"
USER_NAME="${3:-cuzic}"
ITERS="${4:-30}"
TMPFILE="$(mktemp "${TMPDIR:-/tmp}/ssh_latency_XXXXXX.txt")"
trap 'rm -f "$TMPFILE"' EXIT

echo "=== TCP SSH Latency Measurement ==="
echo "Target     : ${USER_NAME}@${HOST}:${PORT}"
echo "Iterations : $ITERS"
echo ""

# --- 1. TCP 接続レイテンシ ---
echo "--- TCP Connect Latency ---"
if command -v nc &>/dev/null; then
    for i in $(seq 1 10); do
        start=$(date +%s%N)
        if nc -zw2 "$HOST" "$PORT" 2>/dev/null; then
            echo "  tcp_connect_ms=$(( ($(date +%s%N) - start) / 1000000 ))"
        else
            echo "  tcp_connect: FAILED (iter $i)"
        fi
    done
else
    echo "  nc not found — skipping TCP connect measurement"
fi

# --- 2. SSH 往復レイテンシ（既存接続の多重化を避けるため毎回新規接続）---
echo ""
echo "--- SSH Round-trip Latency (connect + 'echo ok' + teardown) ---"
SSH_OPTS=(-o StrictHostKeyChecking=no -o ConnectTimeout=5 -o BatchMode=yes -o ControlMaster=no)
: > "$TMPFILE"
fail=0
for i in $(seq 1 "$ITERS"); do
    start=$(date +%s%N)
    if ssh "${SSH_OPTS[@]}" -p "$PORT" "${USER_NAME}@${HOST}" 'echo ok' >/dev/null 2>&1; then
        elapsed=$(( ($(date +%s%N) - start) / 1000000 ))
        echo "$elapsed" >> "$TMPFILE"
        printf "  iter %2d: %d ms\n" "$i" "$elapsed"
    else
        fail=$((fail + 1))
        printf "  iter %2d: FAILED\n" "$i"
    fi
done

# --- 3. 統計 ---
echo ""
echo "--- Statistics (ms) ---"
if [ -s "$TMPFILE" ]; then
    sort -n "$TMPFILE" | awk '
        { vals[NR]=$1; sum+=$1; if(NR==1||$1<min)min=$1; if($1>max)max=$1 }
        END {
            n=NR
            printf "  samples: %d\n", n
            printf "  min: %d ms\n", min
            printf "  avg: %.1f ms\n", sum/n
            printf "  p50: %d ms\n", vals[int((n-1)*0.50)+1]
            printf "  p95: %d ms\n", vals[int((n-1)*0.95)+1]
            printf "  max: %d ms\n", max
        }
    '
else
    echo "  no successful samples"
fi
[ "$fail" -gt 0 ] && echo "  failures: $fail / $ITERS"

echo ""
echo "=== Phase 5 判断ゲート ==="
echo "  - TCP SSH 接続が 5G→WiFi 切替で実際に切れるか実機で確認すること"
echo "  - 切れない（TCP が生存する）なら Phase 5（tsshd/QUIC）はスキップ"
echo "  - QUIC 採用基準: 接続移行オーバーヘッド < 100ms / steady-state RTT は TCP の ±10% 以内"
