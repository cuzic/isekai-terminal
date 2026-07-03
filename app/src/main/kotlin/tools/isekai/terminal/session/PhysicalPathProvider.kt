package tools.isekai.terminal.session

import android.content.Context
import android.net.ConnectivityManager
import android.net.Network
import android.net.NetworkCapabilities
import android.net.NetworkRequest
import android.os.ParcelFileDescriptor
import java.net.DatagramSocket
import java.net.Inet4Address
import java.net.InetSocketAddress
import kotlin.coroutines.resume
import kotlinx.coroutines.suspendCancellableCoroutine
import kotlinx.coroutines.withTimeoutOrNull
import tools.isekai.terminal.util.RemoteLogger

/**
 * Phase 9-4（実験的機能、既定 OFF）: Wi-Fi / セルラー物理無線それぞれに
 * [Network.bindSocket] で明示的にバインドした UDP ソケットの生 fd を取得する。
 *
 * Tailscale 稼働中は [Network.bindSocket] 自体が VPN ロックで `EPERM` になる
 * （実機検証済み、PLAN.md Phase 7-7）。その場合はここで例外を握りつぶして
 * 該当の片方（または両方）を `null` にするだけで、呼び出し側は特別分岐しない
 * （日和見的ポリシー、既存メモリ `multipath-opportunistic-policy` と同じ考え方）。
 *
 * fd の所有権注意点（実機スパイクで確認済みの罠）: [ParcelFileDescriptor.detachFd]
 * （`.fd` ではない）で fdsan の Java 側所有権タグを外す必要がある。外さないまま
 * Rust 側 `UdpSocket::from_raw_fd()` が drop 時に close すると、fdsan が
 * 「まだ ParcelFileDescriptor が所有しているはずの fd を close した」と判断して
 * プロセスを abort する。
 */
data class PhysicalMultipathFds(
    val wifiFd: Int? = null,
    val wifiLocalIp: String? = null,
    val cellularFd: Int? = null,
    val cellularLocalIp: String? = null,
)

class PhysicalPathProvider(context: Context) {
    private val cm = context.getSystemService(Context.CONNECTIVITY_SERVICE) as ConnectivityManager
    private val callbacks = mutableListOf<ConnectivityManager.NetworkCallback>()

    /**
     * Wi-Fi・セルラー双方について並行して [Network.bindSocket] を試み、
     * 成功した方だけ fd + ローカル IP を返す。両方失敗しても例外は投げない
     * （呼び出し側は物理 path 無しで path0/path1 のみにフォールバックする）。
     *
     * ここで登録した [ConnectivityManager.NetworkRequest] は [release] まで
     * 維持する（＝該当の無線を要求し続ける）。接続が終わったら必ず [release] を
     * 呼ぶこと——呼ばないと無線をアプリが握り続け、バッテリーを消費する。
     */
    suspend fun acquire(timeoutMs: Long = 5000): PhysicalMultipathFds {
        val wifi = acquireOne("wifi", NetworkCapabilities.TRANSPORT_WIFI, timeoutMs)
        val cellular = acquireOne("cellular", NetworkCapabilities.TRANSPORT_CELLULAR, timeoutMs)
        return PhysicalMultipathFds(
            wifiFd = wifi?.first,
            wifiLocalIp = wifi?.second,
            cellularFd = cellular?.first,
            cellularLocalIp = cellular?.second,
        )
    }

    /**
     * セルラーだけをbindSocketして生fd+ローカルIPを取得する（[acquire]のセルラー単体版）。
     * 「WiFiのupstreamが死んでいる」検知時のrebind先取得に使う。
     */
    suspend fun acquireCellularOnly(timeoutMs: Long = 5000): Pair<Int, String>? =
        acquireOne("cellular", NetworkCapabilities.TRANSPORT_CELLULAR, timeoutMs)

    private suspend fun acquireOne(label: String, transport: Int, timeoutMs: Long): Pair<Int, String>? {
        val network = awaitNetwork(transport, timeoutMs)
        if (network == null) {
            RemoteLogger.i("PhysicalPath", "$label: network not available within ${timeoutMs}ms, skipping")
            return null
        }
        return try {
            bindAndDetach(network).also {
                RemoteLogger.i("PhysicalPath", "$label: bound fd=${it.first} localIp=${it.second}")
            }
        } catch (e: Exception) {
            // Tailscale 稼働中の EPERM 等はここに来る。想定内なので warn で留める。
            RemoteLogger.w("PhysicalPath", "$label: bindSocket failed (${e.javaClass.simpleName}: ${e.message})")
            null
        }
    }

    private suspend fun awaitNetwork(transport: Int, timeoutMs: Long): Network? =
        withTimeoutOrNull(timeoutMs) {
            suspendCancellableCoroutine { cont ->
                val request = NetworkRequest.Builder()
                    .addCapability(NetworkCapabilities.NET_CAPABILITY_INTERNET)
                    .addTransportType(transport)
                    .build()
                val callback = object : ConnectivityManager.NetworkCallback() {
                    override fun onAvailable(network: Network) {
                        if (cont.isActive) cont.resume(network)
                    }
                }
                synchronized(callbacks) { callbacks.add(callback) }
                cm.requestNetwork(request, callback)
            }
        }

    private fun bindAndDetach(network: Network): Pair<Int, String> {
        // socket.bind(InetSocketAddress(0))（ワイルドカードbind）だと、この端末のような
        // デュアルスタック環境では実機検証でIPv6ワイルドカード(::)が選ばれてしまい、
        // dumpsys connectivityが示す実際のIPv4アドレス（例: 192.168.10.80/24）を
        // 取得できなかった（実機検証で発見、2026-07-03）。LinkPropertiesから明示的に
        // IPv4アドレスを取得し、そのアドレスへ直接bindする。
        val ipv4 = cm.getLinkProperties(network)?.linkAddresses
            ?.map { it.address }
            ?.filterIsInstance<Inet4Address>()
            ?.firstOrNull()
            ?: error("no IPv4 link address on network (IPv6-only network, unsupported yet)")
        val socket = DatagramSocket(null)
        network.bindSocket(socket)
        socket.bind(InetSocketAddress(ipv4, 0))
        val fd = ParcelFileDescriptor.fromDatagramSocket(socket).detachFd()
        return fd to ipv4.hostAddress!!
    }

    /** 保持していたネットワークリクエストをすべて解除する。接続終了時に必ず呼ぶこと。 */
    fun release() {
        synchronized(callbacks) {
            callbacks.forEach { runCatching { cm.unregisterNetworkCallback(it) } }
            callbacks.clear()
        }
    }
}
