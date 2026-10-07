#!/usr/bin/env bash
# L2シナリオ3: server側zombie relay(857f6ae6 D-2型)。
# 旧QUIC接続の4タプルだけを両方向DROPする。クライアントは15sのidle timeoutで死亡を検出して
# 新しいソケット(別ポート)でRESUMEするが、serveは--idle-timeout 60なので旧接続を
# Establishedのまま(=zombie)保持している。serveが旧relayをpreemptして新接続を受理し、
# fencing slotが詰まらないことを検証する(BUSY_OTHER_SESSIONで締め出されると完走しない)。
# 旧ポートは、injection時点でclient nsの`isekai-pipe connect`が持つUDPソケットから取る。

NETLAB_SCENARIO_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=../common.sh
source "$NETLAB_SCENARIO_DIR/../common.sh"

inject_zombie() {
    local ports port
    ports="$(ip netns exec "$NETLAB_CLIENT_NS" ss -uHnap 2>/dev/null \
        | awk '/isekai-pipe/ {n=split($4, a, ":"); print a[n]}' | sort -u)"
    if [ -z "$ports" ]; then
        echo "could not find the client's QUIC UDP socket" >&2
        ip netns exec "$NETLAB_CLIENT_NS" ss -uanp >&2 || true
        return 1
    fi
    echo "== old client UDP ports: $(echo $ports) =="
    for port in $ports; do
        ip netns exec "$NETLAB_SERVER_NS" iptables -I INPUT -p udp --sport "$port" -j DROP
        ip netns exec "$NETLAB_SERVER_NS" iptables -I OUTPUT -p udp --dport "$port" -j DROP
    done
    # ルールは旧ポート専用なので、新ソケットでのresumeは通る。ssh完了まで維持する
    # (cleanupがnetnsごと破棄する)。
}

netlab_up
netlab_stack_up --idle-timeout 60
netlab_run_paced_scenario 70 inject_zombie
