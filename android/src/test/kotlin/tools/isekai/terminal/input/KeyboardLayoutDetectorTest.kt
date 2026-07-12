package tools.isekai.terminal.input

import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Test
import org.junit.runner.RunWith
import org.robolectric.RobolectricTestRunner
import org.robolectric.annotation.Config

/**
 * [KeyboardLayoutDetector.isJisKeyboard] は実機の`InputDevice`(接続中の外部キーボードの
 * HID構成)に依存するため、Robolectric環境で「本物のJIS配列デバイス」を再現して自動検出
 * ロジック自体を検証することはできない([InputDevice]はfinalかつファクトリがフレームワーク
 * 内部にしか無く、テストから物理キー構成を持つインスタンスを組み立てる手段が無い)。
 * ここでは (1) デバイス不明時のnullセーフな既定動作 と (2) [KeyboardLayoutMode]による
 * 明示上書き(JIS/US)がデバイス問い合わせを経由せず確定することの2点のみを検証する。
 * 実機でのJIS配列キーボードによる自動検出そのものは、実機での動作確認が必要
 * （完了報告に記載）。
 */
@RunWith(RobolectricTestRunner::class)
@Config(sdk = [28])
class KeyboardLayoutDetectorTest {

    @Test
    fun isJisKeyboard_nullDevice_returnsFalse() {
        assertFalse(KeyboardLayoutDetector.isJisKeyboard(null))
    }

    @Test
    fun resolveJisLayout_modeJis_alwaysTrue_regardlessOfDevice() {
        assertEquals(true, KeyboardLayoutDetector.resolveJisLayout(KeyboardLayoutMode.JIS, null))
    }

    @Test
    fun resolveJisLayout_modeUs_alwaysFalse_regardlessOfDevice() {
        assertEquals(false, KeyboardLayoutDetector.resolveJisLayout(KeyboardLayoutMode.US, null))
    }

    @Test
    fun resolveJisLayout_modeAuto_withNullDevice_fallsBackToFalse() {
        assertEquals(false, KeyboardLayoutDetector.resolveJisLayout(KeyboardLayoutMode.AUTO, null))
    }
}
