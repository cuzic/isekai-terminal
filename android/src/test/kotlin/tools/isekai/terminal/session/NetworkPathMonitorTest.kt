package tools.isekai.terminal.session

import android.content.Context
import android.net.ConnectivityManager
import android.net.Network
import androidx.test.core.app.ApplicationProvider
import org.junit.Assert.assertEquals
import org.junit.Before
import org.junit.Test
import org.junit.runner.RunWith
import org.robolectric.RobolectricTestRunner
import org.robolectric.Shadows.shadowOf
import org.robolectric.annotation.Config
import org.robolectric.shadow.api.Shadow
import org.robolectric.shadows.ShadowNetwork

@RunWith(RobolectricTestRunner::class)
@Config(sdk = [33])
class NetworkPathMonitorTest {

    private lateinit var connectivityManager: ConnectivityManager
    private lateinit var monitor: NetworkPathMonitor

    @Before
    fun setup() {
        val context = ApplicationProvider.getApplicationContext<Context>()
        connectivityManager =
            context.getSystemService(Context.CONNECTIVITY_SERVICE) as ConnectivityManager
        monitor = NetworkPathMonitor(connectivityManager)
    }

    @Test
    fun bothPathsStartUnknown() {
        monitor.start()
        assertEquals(PathState.UNKNOWN, monitor.currentState(PathId.DIRECT))
        assertEquals(PathState.UNKNOWN, monitor.currentState(PathId.TAILSCALE))
    }

    @Test
    fun pathsBecomeValidatedWhenTheirNetworkBecomesAvailable() {
        monitor.start()
        val network = Shadow.newInstanceOf(Network::class.java)

        shadowOf(connectivityManager).networkCallbacks.forEach { it.onAvailable(network) }

        assertEquals(PathState.VALIDATED, monitor.currentState(PathId.DIRECT))
        assertEquals(PathState.VALIDATED, monitor.currentState(PathId.TAILSCALE))
    }

    @Test
    fun pathsBecomeFailedWhenTheirNetworkIsLost() {
        monitor.start()
        val network = Shadow.newInstanceOf(Network::class.java)
        val callbacks = shadowOf(connectivityManager).networkCallbacks
        callbacks.forEach { it.onAvailable(network) }

        callbacks.forEach { it.onLost(network) }

        assertEquals(PathState.FAILED, monitor.currentState(PathId.DIRECT))
        assertEquals(PathState.FAILED, monitor.currentState(PathId.TAILSCALE))
    }

    /** AND-M3a: 同じPathId(DIRECT)に複数ネットワーク(Wi-Fi+セルラー)がいる状態で片方だけ
     *  失っても、残りがある限りFAILEDにせず、誤った「経路なし」を通知しない。 */
    @Test
    fun losingOneOfTwoNetworksOnSamePath_keepsPathValidated() {
        val seen = mutableListOf<Boolean>()
        monitor.start { seen.add(it) }
        val wifi = ShadowNetwork.newInstance(100)
        val cellular = ShadowNetwork.newInstance(101)
        // networkCallbacksの順序は保証されないため、どちらのPathIdのコールバックかは
        // 最初のonAvailable後に状態から特定する(検証内容はPathIdに依らない)。
        val cb = shadowOf(connectivityManager).networkCallbacks.toList()[0]

        cb.onAvailable(wifi)
        val path = PathId.values().single { monitor.currentState(it) == PathState.VALIDATED }
        cb.onAvailable(cellular)
        cb.onLost(wifi)

        assertEquals(PathState.VALIDATED, monitor.currentState(path))
        assertEquals(listOf(true, true, true), seen)

        cb.onLost(cellular)

        assertEquals(PathState.FAILED, monitor.currentState(path))
        assertEquals(false, seen.last())
    }

    @Test
    fun stopUnregistersAllCallbacks() {
        monitor.start()
        assertEquals(2, shadowOf(connectivityManager).networkCallbacks.size)

        monitor.stop()

        assertEquals(0, shadowOf(connectivityManager).networkCallbacks.size)
    }

    @Test
    fun aggregateChangedFiresOnceWhenFirstPathBecomesAvailable() {
        val seen = mutableListOf<Boolean>()
        monitor.start { seen.add(it) }
        val network = Shadow.newInstanceOf(Network::class.java)
        val callbacks = shadowOf(connectivityManager).networkCallbacks.toList()

        callbacks[0].onAvailable(network)

        assertEquals(listOf(true), seen)
    }

    @Test
    fun aggregateChangedStaysTrueUntilTheLastPathIsLost() {
        val seen = mutableListOf<Boolean>()
        monitor.start { seen.add(it) }
        val network = Shadow.newInstanceOf(Network::class.java)
        val callbacks = shadowOf(connectivityManager).networkCallbacks.toList()

        callbacks[0].onAvailable(network)
        callbacks[1].onAvailable(network)
        callbacks[0].onLost(network) // one path still VALIDATED -> aggregate stays true
        callbacks[1].onLost(network) // now both FAILED -> aggregate goes false

        assertEquals(listOf(true, true, true, false), seen)
    }
}
