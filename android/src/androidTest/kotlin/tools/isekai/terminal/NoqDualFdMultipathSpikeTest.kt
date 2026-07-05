package tools.isekai.terminal

/**
 * One-off, throwaway spike: proves that noq's native QUIC multipath can ride
 * a real Wi-Fi-bound fd + a real Cellular-bound fd simultaneously on-device,
 * via `ConnectivityManager.requestNetwork()` + `Network.bindSocket()`.
 *
 * Not part of the app's real feature set -- depends on a manually pushed
 * `/data/local/tmp/libnoq_multipath_spike.so` and a manually run
 * `noq-spike-server` on the dev box, both from the ad-hoc `noq-multipath-spike`
 * Rust crate. Safe to delete once the spike concludes.
 */

import android.content.Context
import android.net.ConnectivityManager
import android.net.Network
import android.net.NetworkCapabilities
import android.net.NetworkRequest
import android.os.ParcelFileDescriptor
import androidx.test.core.app.ApplicationProvider
import androidx.test.ext.junit.runners.AndroidJUnit4
import java.net.DatagramSocket
import java.net.Inet4Address
import java.net.Inet6Address
import java.net.InetAddress
import java.net.InetSocketAddress
import java.util.concurrent.CountDownLatch
import java.util.concurrent.TimeUnit
import org.junit.Test
import org.junit.runner.RunWith

object NoqMultipathSpike {
    private var loaded = false

    // Android 10+ W^X blocks dlopen()ing a .so directly from a world-writable
    // path like /data/local/tmp. Copy it into the app's private (executable)
    // storage first, then load from there.
    fun loadFrom(context: Context) {
        if (loaded) return
        val src = java.io.File("/data/local/tmp/libnoq_multipath_spike.so")
        val dst = java.io.File(context.filesDir, "libnoq_multipath_spike.so")
        src.copyTo(dst, overwrite = true)
        dst.setExecutable(true)
        System.load(dst.absolutePath)
        loaded = true
    }

    external fun runDualFdSpike(
        wifiFd: Int,
        wifiIp: String,
        cellularFd: Int,
        cellularIp: String,
        directAddr: String,
        tailscaleAddr: String,
        certPath: String,
        serverName: String,
    ): String
}

@RunWith(AndroidJUnit4::class)
class NoqDualFdMultipathSpikeTest {

    private fun awaitNetwork(cm: ConnectivityManager, transport: Int): Network {
        val latch = CountDownLatch(1)
        var result: Network? = null
        val request = NetworkRequest.Builder()
            .addCapability(NetworkCapabilities.NET_CAPABILITY_INTERNET)
            .addTransportType(transport)
            .build()
        val callback = object : ConnectivityManager.NetworkCallback() {
            override fun onAvailable(network: Network) {
                result = network
                latch.countDown()
            }
        }
        cm.requestNetwork(request, callback)
        latch.await(15, TimeUnit.SECONDS)
        return result ?: error("network for transport=$transport not available within timeout")
    }

    private fun localIpv4Of(cm: ConnectivityManager, network: Network): String {
        val props = cm.getLinkProperties(network) ?: error("no LinkProperties for $network")
        val addr = props.linkAddresses.map { it.address }.firstOrNull { it is Inet4Address }
            ?: error("no IPv4 address on $network")
        return addr.hostAddress!!
    }

    // This AP hands out IPv6 only (no IPv4 lease), so the Wi-Fi path uses its
    // global IPv6 address instead -- see dualFdMultipath_wifiPlusCellular.
    private fun localIpv6Of(cm: ConnectivityManager, network: Network): String {
        val props = cm.getLinkProperties(network) ?: error("no LinkProperties for $network")
        val addr = props.linkAddresses.map { it.address }
            .firstOrNull { it is Inet6Address && !it.isLinkLocalAddress }
            ?: error("no global IPv6 address on $network")
        return addr.hostAddress!!
    }

    @Test
    fun dualFdMultipath_wifiPlusCellular() {
        val context = ApplicationProvider.getApplicationContext<Context>()
        NoqMultipathSpike.loadFrom(context)
        val cm = context.getSystemService(Context.CONNECTIVITY_SERVICE) as ConnectivityManager

        val wifiNetwork = awaitNetwork(cm, NetworkCapabilities.TRANSPORT_WIFI)
        val cellularNetwork = awaitNetwork(cm, NetworkCapabilities.TRANSPORT_CELLULAR)
        val wifiIp = localIpv6Of(cm, wifiNetwork)
        val cellularIp = localIpv4Of(cm, cellularNetwork)

        // Tailscale OFF verification run: now using the *real* Network.bindSocket()
        // API for both paths (not the raw local-IP bind() workaround). With no
        // VPN active, this UID shouldn't be VPN-locked, so bindSocket() to a
        // physical Network should succeed instead of EPERM -- and, unlike the
        // raw-bind workaround (confirmed via /proc/net/dev to NOT actually route
        // over the bound interface), bindSocket() is the API that genuinely pins
        // a socket's traffic to a specific Network.
        //
        // The Wi-Fi network here has no IPv4 (router hands out IPv6 only), so
        // path0 (wifi) targets the dev box's HE Tunnelbroker IPv6 address
        // instead of its IPv4 one; path1 (cellular) still uses IPv4. The dev
        // box's noq-spike-server binds dual-stack ([::], bindv6only=0) so both
        // paths land on the same server Connection.
        val wifiSocket = DatagramSocket(null)
        wifiNetwork.bindSocket(wifiSocket)
        wifiSocket.bind(InetSocketAddress(0))
        val cellularSocket = DatagramSocket(null)
        cellularNetwork.bindSocket(cellularSocket)
        cellularSocket.bind(InetSocketAddress(0))

        // detachFd() (not .fd) removes fdsan's Java-side ownership tag on the fd.
        // Without this, Rust's UdpSocket::from_raw_fd() takes ownership and later
        // closes it on drop, which fdsan flags as "closing an fd still owned by a
        // ParcelFileDescriptor" and aborts the process (confirmed on-device).
        val wifiFdRaw = ParcelFileDescriptor.fromDatagramSocket(wifiSocket).detachFd()
        val cellularFdRaw = ParcelFileDescriptor.fromDatagramSocket(cellularSocket).detachFd()

        android.util.Log.i("NoqSpike", "wifiIp=$wifiIp cellularIp=$cellularIp")

        val result = NoqMultipathSpike.runDualFdSpike(
            wifiFdRaw,
            wifiIp,
            cellularFdRaw,
            cellularIp,
            "204.12.203.210:45820", // dev box, direct public IPv4 -- path1 (cellular)
            "[2001:470:23:47b::2]:45820", // same dev box, HE Tunnelbroker IPv6 -- path0 (wifi, no IPv4 on this AP)
            "/data/local/tmp/noq-spike-cert.der",
            "noq-spike",
        )

        android.util.Log.i("NoqSpike", "result:\n$result")
        println("=== NoqSpike result ===\n$result\n=== end ===")
    }

    /**
     * Bypasses QUIC/noq entirely: plain UDP echo over the cellular-bound
     * socket, sweeping payload sizes, to check whether the cellular return
     * path is failing for ALL sizes or specifically for QUIC-sized (~1200
     * byte) datagrams (MTU/fragmentation vs a blanket block).
     */
    @Test
    fun cellularUdpEcho_sizeSweep() {
        val context = ApplicationProvider.getApplicationContext<Context>()
        val cm = context.getSystemService(Context.CONNECTIVITY_SERVICE) as ConnectivityManager
        val cellularNetwork = awaitNetwork(cm, NetworkCapabilities.TRANSPORT_CELLULAR)
        val cellularIp = localIpv4Of(cm, cellularNetwork)
        android.util.Log.i("NoqSpike", "cellularIp=$cellularIp")

        val echoServer = InetSocketAddress(InetAddress.getByName("34.85.105.222"), 45821)
        val sizes = listOf(32, 128, 512, 1000, 1200, 1350)

        for (size in sizes) {
            val socket = DatagramSocket(InetSocketAddress(InetAddress.getByName(cellularIp), 0))
            socket.soTimeout = 3000
            try {
                val payload = ByteArray(size) { 'A'.code.toByte() }
                val packet = java.net.DatagramPacket(payload, payload.size, echoServer)
                socket.send(packet)
                val replyBuf = ByteArray(2000)
                val replyPacket = java.net.DatagramPacket(replyBuf, replyBuf.size)
                socket.receive(replyPacket)
                android.util.Log.i(
                    "NoqSpike",
                    "size=$size OK: got ${replyPacket.length} bytes back from ${replyPacket.address}",
                )
            } catch (e: Exception) {
                android.util.Log.i("NoqSpike", "size=$size FAILED: ${e.javaClass.simpleName}: ${e.message}")
            } finally {
                socket.close()
            }
        }
    }

    /**
     * Doesn't need Wi-Fi at all: proves whether the *real* Network.bindSocket()
     * API (not the raw local-IP bind() workaround) actually pins a UDP socket's
     * traffic to the cellular radio. Requires Tailscale to be off (bindSocket()
     * to a physical Network is VPN-locked -> EPERM otherwise, confirmed
     * separately). Plain UDP echo, no QUIC/noq involved.
     */
    @Test
    fun cellularBindSocket_udpEcho() {
        val context = ApplicationProvider.getApplicationContext<Context>()
        val cm = context.getSystemService(Context.CONNECTIVITY_SERVICE) as ConnectivityManager
        val cellularNetwork = awaitNetwork(cm, NetworkCapabilities.TRANSPORT_CELLULAR)

        val socket = DatagramSocket(null)
        cellularNetwork.bindSocket(socket)
        socket.bind(InetSocketAddress(0))
        socket.soTimeout = 5000

        val echoServer = InetSocketAddress(InetAddress.getByName("204.12.203.210"), 45822)
        try {
            val payload = "hello via Network.bindSocket(cellular)".toByteArray()
            socket.send(java.net.DatagramPacket(payload, payload.size, echoServer))
            val replyBuf = ByteArray(2000)
            val replyPacket = java.net.DatagramPacket(replyBuf, replyBuf.size)
            socket.receive(replyPacket)
            android.util.Log.i(
                "NoqSpike",
                "bindSocket echo OK: got ${replyPacket.length} bytes back from ${replyPacket.address}",
            )
        } catch (e: Exception) {
            android.util.Log.i("NoqSpike", "bindSocket echo FAILED: ${e.javaClass.simpleName}: ${e.message}")
        } finally {
            socket.close()
        }
    }
}
