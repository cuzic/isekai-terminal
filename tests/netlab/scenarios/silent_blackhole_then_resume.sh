#!/usr/bin/env bash
# L2シナリオ1: サイレントblackhole → 復旧後にresumeして同一SSHセッションが完走する。
# docs/adr/0018-connection-resilience-simulation.md L2 / docs/adr/0021-deterministic-network-simulation-l1.md §5-4。
#
# 両netnsでUDPをiptables DROP(エラー応答なし=サイレント)し、QUIC idle timeout(15s)を
# 超える25秒維持してから解除する。その間もsshのstdinにはデータが流れ続ける。
# 検証: sha256一致(バイト損失/重複なし)かつserveが2本目以降のQUIC接続を受けた(=実際にresume)。
# ログに「ssh完了までの秒数」を出す(RES ADR Q8: 実QUICの死亡検出+復旧時間の実測)。

NETLAB_SCENARIO_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=../common.sh
source "$NETLAB_SCENARIO_DIR/../common.sh"

inject_blackhole() {
    local ns
    for ns in "$NETLAB_CLIENT_NS" "$NETLAB_SERVER_NS"; do
        ip netns exec "$ns" iptables -I INPUT -p udp -j DROP
        ip netns exec "$ns" iptables -I OUTPUT -p udp -j DROP
    done
    sleep "${BLACKHOLE_SECS:-25}"
    for ns in "$NETLAB_CLIENT_NS" "$NETLAB_SERVER_NS"; do
        ip netns exec "$ns" iptables -D INPUT -p udp -j DROP
        ip netns exec "$ns" iptables -D OUTPUT -p udp -j DROP
    done
    echo "== [$(date +%T)] blackhole lifted =="
}

netlab_up
netlab_stack_up
netlab_run_paced_scenario 75 inject_blackhole
