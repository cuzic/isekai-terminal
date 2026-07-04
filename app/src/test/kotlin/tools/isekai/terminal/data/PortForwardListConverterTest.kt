package tools.isekai.terminal.data

import android.os.Parcel
import android.os.Parcelable
import org.junit.Assert.assertEquals
import org.junit.Test
import org.junit.runner.RunWith
import org.robolectric.RobolectricTestRunner
import org.robolectric.annotation.Config
import uniffi.tssh_core.ForwardType
import uniffi.tssh_core.PortForward

/**
 * [PortForwardListConverter](Room TypeConverter)と[PortForwardParceler](`@Parcelize`用)を
 * UI/Roomを介さず直接検証する。過去に両者ともLOCAL固定でforwardTypeを保存しないMVP実装の
 * まま放置されており、Remote/Dynamicで保存してもDBラウンドトリップ後にLocalへ化けるバグが
 * あった(instrumented testで発覚・修正済み、Phase 12 P2-2)。このテストはそのバグの
 * 再発を直接ピン留めする。
 */
@RunWith(RobolectricTestRunner::class)
@Config(sdk = [28])
class PortForwardListConverterTest {

    // ── PortForwardListConverter(Room TypeConverter) ──────────────────────

    @Test fun `toForwards of empty json returns empty list`() {
        assertEquals(emptyList<PortForward>(), PortForwardListConverter.toForwards(""))
        assertEquals(emptyList<PortForward>(), PortForwardListConverter.toForwards("[]"))
    }

    @Test fun `fromForwards then toForwards round-trips LOCAL forward`() {
        val forward = PortForward(
            forwardType = ForwardType.LOCAL,
            bindAddress = "127.0.0.1", bindPort = 8080u,
            remoteHost = "internal.example.com", remotePort = 80u,
        )
        val restored = PortForwardListConverter.toForwards(PortForwardListConverter.fromForwards(listOf(forward)))
        assertEquals(listOf(forward), restored)
    }

    @Test fun `fromForwards then toForwards round-trips REMOTE forward`() {
        val forward = PortForward(
            forwardType = ForwardType.REMOTE,
            bindAddress = "0.0.0.0", bindPort = 9090u,
            remoteHost = "192.168.1.5", remotePort = 22u,
        )
        val restored = PortForwardListConverter.toForwards(PortForwardListConverter.fromForwards(listOf(forward)))
        assertEquals(listOf(forward), restored)
        assertEquals(ForwardType.REMOTE, restored[0].forwardType)
    }

    @Test fun `fromForwards then toForwards round-trips DYNAMIC forward`() {
        val forward = PortForward(
            forwardType = ForwardType.DYNAMIC,
            bindAddress = "127.0.0.1", bindPort = 1080u,
            remoteHost = "", remotePort = 0u,
        )
        val restored = PortForwardListConverter.toForwards(PortForwardListConverter.fromForwards(listOf(forward)))
        assertEquals(listOf(forward), restored)
        assertEquals(ForwardType.DYNAMIC, restored[0].forwardType)
    }

    @Test fun `fromForwards then toForwards round-trips a mix of all three forward types`() {
        val forwards = listOf(
            PortForward(ForwardType.LOCAL, "127.0.0.1", 8080u, "a.example.com", 80u),
            PortForward(ForwardType.REMOTE, "0.0.0.0", 9090u, "192.168.1.5", 22u),
            PortForward(ForwardType.DYNAMIC, "127.0.0.1", 1080u, "", 0u),
        )
        val restored = PortForwardListConverter.toForwards(PortForwardListConverter.fromForwards(forwards))
        assertEquals(forwards, restored)
    }

    @Test fun `toForwards defaults to LOCAL when the type field is missing (legacy JSON without a type key)`() {
        // MVP時代(forwardType追加以前)に保存されたJSONを想定(typeキー自体が無い)。
        val legacyJson = """[{"bindAddress":"127.0.0.1","bindPort":8080,"remoteHost":"a.example.com","remotePort":80}]"""
        val restored = PortForwardListConverter.toForwards(legacyJson)
        assertEquals(ForwardType.LOCAL, restored[0].forwardType)
    }

    @Test fun `toForwards defaults to LOCAL when the type field is an unknown value`() {
        val json = """[{"type":"BOGUS","bindAddress":"127.0.0.1","bindPort":8080,"remoteHost":"a.example.com","remotePort":80}]"""
        val restored = PortForwardListConverter.toForwards(json)
        assertEquals(ForwardType.LOCAL, restored[0].forwardType)
    }

    // ── PortForwardParceler(@Parcelize用) ─────────────────────────────────

    // kotlin-parcelizeが生成するCREATORはJavaからは`ConnectionProfile.CREATOR`で見えるが、
    // 別コンパイル単位(このテストモジュール)のKotlinフロントエンドからは合成メンバーとして
    // 解決できない既知の制約があるため、リフレクション経由で取得する。
    @Suppress("UNCHECKED_CAST")
    private fun connectionProfileCreator(): Parcelable.Creator<ConnectionProfile> =
        ConnectionProfile::class.java.getField("CREATOR").get(null) as Parcelable.Creator<ConnectionProfile>

    private fun roundTripThroughParcel(profile: ConnectionProfile): ConnectionProfile {
        val parcel = Parcel.obtain()
        try {
            profile.writeToParcel(parcel, 0)
            parcel.setDataPosition(0)
            return connectionProfileCreator().createFromParcel(parcel)
        } finally {
            parcel.recycle()
        }
    }

    private fun profile(forwards: List<PortForward>) = ConnectionProfile(
        label = "web", host = "example.com", username = "user", authType = "password",
        forwards = forwards,
    )

    @Test fun `Parcelable round-trip preserves REMOTE forwardType`() {
        val forward = PortForward(ForwardType.REMOTE, "0.0.0.0", 9090u, "192.168.1.5", 22u)
        val restored = roundTripThroughParcel(profile(listOf(forward)))
        assertEquals(listOf(forward), restored.forwards)
    }

    @Test fun `Parcelable round-trip preserves DYNAMIC forward without a target`() {
        val forward = PortForward(ForwardType.DYNAMIC, "127.0.0.1", 1080u, "", 0u)
        val restored = roundTripThroughParcel(profile(listOf(forward)))
        assertEquals(listOf(forward), restored.forwards)
    }

    @Test fun `Parcelable round-trip preserves a mix of all three forward types in order`() {
        val forwards = listOf(
            PortForward(ForwardType.LOCAL, "127.0.0.1", 8080u, "a.example.com", 80u),
            PortForward(ForwardType.REMOTE, "0.0.0.0", 9090u, "192.168.1.5", 22u),
            PortForward(ForwardType.DYNAMIC, "127.0.0.1", 1080u, "", 0u),
        )
        val restored = roundTripThroughParcel(profile(forwards))
        assertEquals(forwards, restored.forwards)
    }
}
