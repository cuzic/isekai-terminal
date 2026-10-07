package tools.isekai.terminal.session

import android.net.ConnectivityManager
import android.net.Network
import android.net.NetworkCapabilities
import android.net.NetworkRequest
import tools.isekai.terminal.util.DebugReconnectLog

/**
 * PLAN.md Phase 7-7 の path broker 構想における `PathState`。
 *
 * このクラスが実際に到達できるのは [VALIDATED]（対応するネットワークが存在する）と
 * [FAILED]（失われた）のみ。[PROBING]/[DEGRADED]/[COOLDOWN] は QUIC 層での実プローブが
 * 必要な状態であり、その配線は別フェーズで行う（ここでは型として先取りしているだけ）。
 */
enum class PathState { UNKNOWN, PROBING, VALIDATED, DEGRADED, FAILED, COOLDOWN }

/** PLAN.md Phase 7-7 の `PathCandidate`。まず direct と tailscale の2種類を対象にする。 */
enum class PathId { DIRECT, TAILSCALE }

/**
 * [PathId] ごとに、対応するネットワークが今 [ConnectivityManager] 上で到達可能かを追跡する。
 *
 * DIRECT は「VPN でない、インターネット到達可能なネットワーク」（Wi-Fi・セルラーいずれか）、
 * TAILSCALE は「VPN トランスポートのネットワーク」の存在を、それぞれの経路が使える目安とする
 * （Tailscale 固有の判定ではなく VPN 全般の存在で代用している点は簡略化）。
 */
class NetworkPathMonitor(private val connectivityManager: ConnectivityManager) {

    private val states = mutableMapOf(
        PathId.DIRECT to PathState.UNKNOWN,
        PathId.TAILSCALE to PathState.UNKNOWN,
    )

    /**
     * AND-M3a: [PathId]ごとに「今そのPathIdの条件を満たすネットワーク」の集合。
     * `registerNetworkCallback`は条件に合う**全ネットワーク**を個別に報告するため、
     * 以前のように`onLost(network)`1回でPathId全体を[PathState.FAILED]にすると、
     * Wi-Fiとセルラーが両方いる状態でWi-Fiだけ失ったときにDIRECTがFAILEDに張り付き
     * (セルラーは既にavailable済みなので再度`onAvailable`は来ない)、誤った「経路なし」を
     * Rustへ送っていた。集合が空になった時だけFAILEDにする。
     */
    private val networks = mutableMapOf<PathId, MutableSet<Network>>(
        PathId.DIRECT to mutableSetOf(),
        PathId.TAILSCALE to mutableSetOf(),
    )
    private val lock = Any()

    private val callbacks = mutableMapOf<PathId, ConnectivityManager.NetworkCallback>()
    private var onAggregateChanged: (Boolean) -> Unit = {}

    fun currentState(id: PathId): PathState = synchronized(lock) { states.getValue(id) }

    /** True if at least one path (direct or Tailscale) currently has a reachable network. */
    fun isAnyPathAvailable(): Boolean = synchronized(lock) { states.values.any { it == PathState.VALIDATED } }

    /**
     * Starts monitoring. [onAggregateChanged] fires with [isAnyPathAvailable] every time any
     * path's state changes, so callers that only care about "is there a usable route at all"
     * (e.g. [AndroidAppExecutor]'s network-lost notification) don't need to track individual
     * paths themselves.
     */
    fun start(onAggregateChanged: (anyPathAvailable: Boolean) -> Unit) {
        this.onAggregateChanged = onAggregateChanged
        register(
            PathId.DIRECT,
            NetworkRequest.Builder()
                .addCapability(NetworkCapabilities.NET_CAPABILITY_INTERNET)
                .addCapability(NetworkCapabilities.NET_CAPABILITY_NOT_VPN)
                .build(),
        )
        register(
            PathId.TAILSCALE,
            NetworkRequest.Builder()
                .addCapability(NetworkCapabilities.NET_CAPABILITY_INTERNET)
                .addTransportType(NetworkCapabilities.TRANSPORT_VPN)
                .build(),
        )
    }

    fun stop() {
        for (callback in callbacks.values) {
            connectivityManager.unregisterNetworkCallback(callback)
        }
        callbacks.clear()
        synchronized(lock) { networks.values.forEach { it.clear() } }
        onAggregateChanged = {}
    }

    private fun register(id: PathId, request: NetworkRequest) {
        val callback = object : ConnectivityManager.NetworkCallback() {
            override fun onAvailable(network: Network) {
                DebugReconnectLog.record("network_callback path=$id callback=onAvailable network=$network")
                val any = synchronized(lock) {
                    networks.getValue(id).add(network)
                    states[id] = PathState.VALIDATED
                    isAnyPathAvailable()
                }
                DebugReconnectLog.record("network_aggregate anyPathAvailable=$any")
                onAggregateChanged(any)
            }

            override fun onLost(network: Network) {
                DebugReconnectLog.record("network_callback path=$id callback=onLost network=$network")
                val any = synchronized(lock) {
                    val remaining = networks.getValue(id).apply { remove(network) }
                    states[id] = if (remaining.isEmpty()) PathState.FAILED else PathState.VALIDATED
                    isAnyPathAvailable()
                }
                DebugReconnectLog.record("network_aggregate anyPathAvailable=$any")
                onAggregateChanged(any)
            }
        }
        connectivityManager.registerNetworkCallback(request, callback)
        callbacks[id] = callback
    }
}
