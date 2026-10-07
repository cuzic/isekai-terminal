#!/usr/bin/env bash
# L2シナリオ2: 完全切断(link down/up) → resumeして同一SSHセッションが完走する。
# サイレントblackholeと違い、送信側にENETUNREACH等のエラーが即座に見える経路
# (EOF/RST系の検出)を通す。ADR_CONNECTION_RESILIENCE_SIMULATION.md L2。

NETLAB_SCENARIO_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=../common.sh
source "$NETLAB_SCENARIO_DIR/../common.sh"

inject_link_down() {
    ip netns exec "$NETLAB_CLIENT_NS" ip link set "$NETLAB_CLIENT_IF" down
    sleep "${LINK_DOWN_SECS:-20}"
    ip netns exec "$NETLAB_CLIENT_NS" ip link set "$NETLAB_CLIENT_IF" up
    echo "== [$(date +%T)] link restored =="
}

netlab_up
netlab_stack_up
netlab_run_paced_scenario 70 inject_link_down
