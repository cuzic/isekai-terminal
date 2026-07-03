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
