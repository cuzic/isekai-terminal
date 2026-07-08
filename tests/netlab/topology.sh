#!/usr/bin/env bash
# 2つのnetwork namespaceをveth1本で直結するだけの最小トポロジー。
#
#   isekai-client (10.201.0.1/30) --- veth-cli === veth-srv --- isekai-server (10.201.0.2/30)
#
# NATや複数経路は扱わない(scenarios/direct_survives_loss_and_delay.shが
# 検証するのは「実バイナリ同士のQUIC直結がtc netemの損失/遅延に耐えるか」
# だけなので、まずはこの最小構成で十分)。root権限が必要(ip netns/veth/tc)。
#
# 使い方:
#   source tests/netlab/topology.sh
#   netlab_up
#   trap netlab_down EXIT
#   ip netns exec "$NETLAB_CLIENT_NS" ...
#
# シェルオプション(set -e等)は呼び出し側で設定すること — sourceされる
# ライブラリの中でsetすると呼び出し側のシェル全体に影響してしまうため
# ここでは触らない。

NETLAB_CLIENT_NS="${NETLAB_CLIENT_NS:-isekai-netlab-client}"
NETLAB_SERVER_NS="${NETLAB_SERVER_NS:-isekai-netlab-server}"
NETLAB_CLIENT_IF="veth-cli"
NETLAB_SERVER_IF="veth-srv"
NETLAB_CLIENT_ADDR="10.201.0.1/30"
NETLAB_SERVER_ADDR="10.201.0.2/30"
NETLAB_SERVER_IP="10.201.0.2"

netlab_up() {
    ip netns add "$NETLAB_CLIENT_NS"
    ip netns add "$NETLAB_SERVER_NS"

    ip link add "$NETLAB_CLIENT_IF" type veth peer name "$NETLAB_SERVER_IF"
    ip link set "$NETLAB_CLIENT_IF" netns "$NETLAB_CLIENT_NS"
    ip link set "$NETLAB_SERVER_IF" netns "$NETLAB_SERVER_NS"

    ip netns exec "$NETLAB_CLIENT_NS" ip addr add "$NETLAB_CLIENT_ADDR" dev "$NETLAB_CLIENT_IF"
    ip netns exec "$NETLAB_CLIENT_NS" ip link set "$NETLAB_CLIENT_IF" up
    ip netns exec "$NETLAB_CLIENT_NS" ip link set lo up

    ip netns exec "$NETLAB_SERVER_NS" ip addr add "$NETLAB_SERVER_ADDR" dev "$NETLAB_SERVER_IF"
    ip netns exec "$NETLAB_SERVER_NS" ip link set "$NETLAB_SERVER_IF" up
    ip netns exec "$NETLAB_SERVER_NS" ip link set lo up
}

# $1=loss(例: "3%") $2=delay(例: "80ms 20ms") 両veth両方向に対称に適用する。
netlab_apply_netem() {
    local loss="$1" delay="$2"
    # $delay は意図的にunquoted(例: "80ms 20ms" を delay/jitterの2引数に
    # word-splitさせるため)。
    # shellcheck disable=SC2086
    ip netns exec "$NETLAB_CLIENT_NS" tc qdisc add dev "$NETLAB_CLIENT_IF" root netem delay $delay loss "$loss"
    # shellcheck disable=SC2086
    ip netns exec "$NETLAB_SERVER_NS" tc qdisc add dev "$NETLAB_SERVER_IF" root netem delay $delay loss "$loss"
}

netlab_down() {
    ip netns del "$NETLAB_CLIENT_NS" 2>/dev/null || true
    ip netns del "$NETLAB_SERVER_NS" 2>/dev/null || true
}

netlab_diagnostics() {
    echo "--- ip netns list ---"
    ip netns list || true
    for ns in "$NETLAB_CLIENT_NS" "$NETLAB_SERVER_NS"; do
        echo "--- $ns: ip addr ---"
        ip netns exec "$ns" ip addr show 2>&1 || true
        echo "--- $ns: tc -s qdisc show ---"
        ip netns exec "$ns" tc -s qdisc show 2>&1 || true
    done
}
