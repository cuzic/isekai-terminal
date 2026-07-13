package tools.isekai.terminal

import org.junit.Assert.assertEquals
import org.junit.Test
import uniffi.isekai_terminal_core.TransportPreference

/**
 * [TransportUiSpec.forPreference]が[TransportPreference]の全値に対して正しい表示条件を
 * 返すことを検証する。以前は`ProfileEditScreen`内に散らばっていた
 * `transportPreference == TransportPreference.X`の羅列条件を集約したもの。
 *
 * [TransportPreference.values()]を走査する形にすることで、将来新しい
 * [TransportPreference]が追加されたときにこのテストで対応漏れを検知しやすくする
 * (`showsHelperAutoDeployNote`が期待通り更新されているかは個別のexpected setで検証)。
 */
class TransportUiSpecTest {

    private val usesHelperAutoDeployNote = setOf(
        TransportPreference.ISEKAI_PIPE_QUIC,
        TransportPreference.AUTO,
        TransportPreference.ISEKAI_PIPE_QUIC_MULTIPATH,
        TransportPreference.ISEKAI_STUN_P2P_QUIC,
        TransportPreference.ISEKAI_LINK_RELAY_QUIC,
    )

    private val usesHelperBindPortField = setOf(
        TransportPreference.ISEKAI_PIPE_QUIC,
        TransportPreference.AUTO,
        TransportPreference.ISEKAI_PIPE_QUIC_MULTIPATH,
    )

    @Test
    fun `showsHelperAutoDeployNote matches the helper-based transports`() {
        for (preference in TransportPreference.values()) {
            assertEquals(
                "showsHelperAutoDeployNote for $preference",
                preference in usesHelperAutoDeployNote,
                TransportUiSpec.forPreference(preference).showsHelperAutoDeployNote,
            )
        }
    }

    @Test
    fun `showsHelperBindPortField matches the direct-helper transports`() {
        for (preference in TransportPreference.values()) {
            assertEquals(
                "showsHelperBindPortField for $preference",
                preference in usesHelperBindPortField,
                TransportUiSpec.forPreference(preference).showsHelperBindPortField,
            )
        }
    }

    @Test
    fun `only ISEKAI_STUN_P2P_QUIC shows stun fields`() {
        for (preference in TransportPreference.values()) {
            assertEquals(
                "showsStunFields for $preference",
                preference == TransportPreference.ISEKAI_STUN_P2P_QUIC,
                TransportUiSpec.forPreference(preference).showsStunFields,
            )
        }
    }

    @Test
    fun `only ISEKAI_LINK_RELAY_QUIC shows relay fields`() {
        for (preference in TransportPreference.values()) {
            assertEquals(
                "showsRelayFields for $preference",
                preference == TransportPreference.ISEKAI_LINK_RELAY_QUIC,
                TransportUiSpec.forPreference(preference).showsRelayFields,
            )
        }
    }

    @Test
    fun `only ISEKAI_PIPE_QUIC_MULTIPATH shows multipath fields`() {
        for (preference in TransportPreference.values()) {
            assertEquals(
                "showsMultipathFields for $preference",
                preference == TransportPreference.ISEKAI_PIPE_QUIC_MULTIPATH,
                TransportUiSpec.forPreference(preference).showsMultipathFields,
            )
        }
    }

    @Test
    fun `only TSSHD_QUIC shows the tsshd port field and sets usesTsshdColumn`() {
        for (preference in TransportPreference.values()) {
            val spec = TransportUiSpec.forPreference(preference)
            val expected = preference == TransportPreference.TSSHD_QUIC
            assertEquals("showsTsshdPortField for $preference", expected, spec.showsTsshdPortField)
            assertEquals("usesTsshdColumn for $preference", expected, spec.usesTsshdColumn)
        }
    }

    @Test
    fun `PLAIN_SSH shows none of the helper-specific fields`() {
        val spec = TransportUiSpec.forPreference(TransportPreference.PLAIN_SSH)
        assertEquals(false, spec.showsHelperAutoDeployNote)
        assertEquals(false, spec.showsHelperBindPortField)
        assertEquals(false, spec.showsStunFields)
        assertEquals(false, spec.showsRelayFields)
        assertEquals(false, spec.showsMultipathFields)
        assertEquals(false, spec.showsTsshdPortField)
        assertEquals(false, spec.usesTsshdColumn)
    }
}
