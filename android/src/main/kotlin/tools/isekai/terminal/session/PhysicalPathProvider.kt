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
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.filterNotNull
import kotlinx.coroutines.flow.first
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

    /**
     * transportごとに1個だけ保持する[ConnectivityManager.NetworkCallback]と、
     * それが最後に報告した[Network]。[watchFor]が[synchronized]付きで
     * transportごとに1回だけ`requestNetwork`する(この無線を要求し続ける状態を
     * 維持しつつ、以後は`onAvailable`/`onLost`で更新されるこの[network]を
     * 読むだけにする)。
     */
    private class TransportWatch(val callback: ConnectivityManager.NetworkCallback) {
        val network = MutableStateFlow<Network?>(null)
    }

    private val watches = mutableMapOf<Int, TransportWatch>()

    /**
     * `transport`用の[TransportWatch]を(まだ無ければ)1回だけ`requestNetwork`して
     * 作る。以前は[acquireOne]を呼ぶたびに新規[NetworkRequest]を登録していたため、
     * `RebindManager`(Rust側)の`ProbeCadence`(10秒周期)でこれを繰り返し呼ぶと、
     * uidあたりの同時[NetworkRequest]数上限(Android既定100件)に約17分で到達し
     * `TooManyRequestsException`が[runBlocking]越しにUniFFIコールバック境界へ
     * 抜けていた(実機未検証のまま存在していたバグ、opusレビューで発見)。
     * transportごとに1回だけ登録し、その後は[TransportWatch.network]を読むだけに
     * することでこの上限に到達しなくなる。
     */
    private fun watchFor(transport: Int): TransportWatch = synchronized(watches) {
        watches.getOrPut(transport) {
            lateinit var watch: TransportWatch
            val callback = object : ConnectivityManager.NetworkCallback() {
                override fun onAvailable(network: Network) {
                    watch.network.value = network
                }
                override fun onLost(network: Network) {
                    if (watch.network.value == network) watch.network.value = null
                }
            }
            watch = TransportWatch(callback)
            val request = NetworkRequest.Builder()
                .addCapability(NetworkCapabilities.NET_CAPABILITY_INTERNET)
                .addTransportType(transport)
                .build()
            cm.requestNetwork(request, callback)
            watch
        }
    }

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

    /**
     * WiFiだけをbindSocketして生fd+ローカルIPを取得する([acquire]のWiFi単体版)。
     * RebindManager(Rust側)がWiFi復帰の疎通確認・実際の復帰rebindのために呼ぶたびに
     * 毎回新規呼び出しする(fd所有権ポリシー: 疎通確認用と本番用は毎回別々に取得する)。
     */
    suspend fun acquireWifiOnly(timeoutMs: Long = 5000): Pair<Int, String>? =
        acquireOne("wifi", NetworkCapabilities.TRANSPORT_WIFI, timeoutMs)

    private suspend fun acquireOne(label: String, transport: Int, timeoutMs: Long): Pair<Int, String>? {
        val network = try {
            awaitNetwork(transport, timeoutMs)
        } catch (e: Exception) {
            // `watchFor`のrequestNetwork自体が例外を投げるケース(理論上のみ、既に
            // transportごとに1回しか呼ばないので`TooManyRequestsException`は
            // もう踏まないはずだが、他の予期しない失敗も含めここで必ず止める —
            // このメソッドはRust側`spawn_blocking`スレッドから同期呼び出しされる
            // 経路の奥にあり、ここで投げるとUniFFIコールバック境界まで例外が
            // 抜けてしまう)。
            RemoteLogger.w("PhysicalPath", "$label: awaitNetwork failed (${e.javaClass.simpleName}: ${e.message})")
            null
        }
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

    private suspend fun awaitNetwork(transport: Int, timeoutMs: Long): Network? {
        val watch = watchFor(transport)
        watch.network.value?.let { return it }
        return withTimeoutOrNull(timeoutMs) { watch.network.filterNotNull().first() }
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
        synchronized(watches) {
            watches.values.forEach { runCatching { cm.unregisterNetworkCallback(it.callback) } }
            watches.clear()
        }
    }
}
