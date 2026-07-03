package tools.isekai.terminal.session

import android.net.ConnectivityManager
import android.net.Network
import android.net.NetworkCapabilities
import android.net.NetworkRequest
import tools.isekai.terminal.util.RemoteLogger

/**
 * 「WiFiは繋がっているがupstream（実際のインターネット到達性）が死んでいる」状態
 * （カフェ等のキャプティブポータル、ルーターのWAN障害）を検知する。
 *
 * [ConnectivityManager.requestNetwork]/[ConnectivityManager.registerNetworkCallback] は
 * 既定で [NetworkCapabilities.NET_CAPABILITY_VALIDATED] を要求扱いするため、この状態の
 * ネットワークに対しては `onAvailable` が来ない。ここでは明示的にVALIDATEDを要求せず
 * WiFiのみを対象にした [NetworkRequest] を使い、`onCapabilitiesChanged` で
 * `hasCapability(NET_CAPABILITY_VALIDATED)` を直接見ることで、
 * 「接続はしているが検証は失敗している」状態そのものを検知する。
 */
class UpstreamHealthMonitor(private val connectivityManager: ConnectivityManager) {
    private var callback: ConnectivityManager.NetworkCallback? = null

    // WiFiが存在しない間はfalseにも書き換えない（「壊れている」という誤検知を避ける）。
    private var wasValidated = true

    /**
     * 監視を開始する。[onWifiUpstreamBroken] はWiFiが「接続はしているが検証失敗」に
     * 転じた瞬間（edge-triggered、連続して呼ばれない）だけ呼ばれる。
     * [onWifiUpstreamRecovered] は検証が回復した瞬間に呼ばれる。
     */
    fun start(onWifiUpstreamBroken: () -> Unit, onWifiUpstreamRecovered: () -> Unit) {
        val request = NetworkRequest.Builder()
            .addTransportType(NetworkCapabilities.TRANSPORT_WIFI)
            .build()
        val cb = object : ConnectivityManager.NetworkCallback() {
            override fun onCapabilitiesChanged(network: Network, capabilities: NetworkCapabilities) {
                val validated = capabilities.hasCapability(NetworkCapabilities.NET_CAPABILITY_VALIDATED)
                if (!validated && wasValidated) {
                    RemoteLogger.w("UpstreamHealth", "WiFi connected but not validated (no real internet)")
                    onWifiUpstreamBroken()
                } else if (validated && !wasValidated) {
                    RemoteLogger.i("UpstreamHealth", "WiFi validated again")
                    onWifiUpstreamRecovered()
                }
                wasValidated = validated
            }

            override fun onLost(network: Network) {
                // WiFi自体が失われただけなら「upstream断」の対象外（別のロジックに委ねる）。
                wasValidated = true
            }
        }
        callback = cb
        connectivityManager.registerNetworkCallback(request, cb)
    }

    fun stop() {
        callback?.let { runCatching { connectivityManager.unregisterNetworkCallback(it) } }
        callback = null
        wasValidated = true
    }
}
