package tools.isekai.terminal.input

import android.view.InputDevice
import android.view.KeyEvent

/**
 * 接続中の外部/BluetoothキーボードがJIS配列かどうかの判定。
 *
 * `KeyCharacterMap`が返す文字自体（`getUnicodeChar()`）はIME/端末が推定した「配列言語」に
 * 依存し、Bluetoothキーボードでは推定が外れることがある。一方 [KeyEvent.KEYCODE_YEN] /
 * [KeyEvent.KEYCODE_RO] はJIS配列にしか物理的に存在しないキーコードのため、
 * `InputDevice.hasKeys()`で「そのキーコードを生成できるキーが物理的に存在するか」を
 * 問い合わせれば、配列推定の成否に関係なくハードウェア構成そのものから判定できる。
 *
 * この判定は「接続されている外部キーボード」というプラットフォームの生の状態を読むだけで、
 * セッション/接続に関する意思決定ではないため、`.claude/rules/rust-ssot.md`が対象とする
 * Rust SSOT原則の対象外（UI入力ロジック）として扱う。判定が外れる端末向けに
 * [KeyboardLayoutMode.JIS]/[KeyboardLayoutMode.US]での手動上書きを用意している
 * （[resolveJisLayout]参照）。
 */
object KeyboardLayoutDetector {

    /** [device] がJIS配列固有キー(¥/ろ)を物理的に備えているか。 */
    fun isJisKeyboard(device: InputDevice?): Boolean {
        if (device == null) return false
        val hasKeys = device.hasKeys(KeyEvent.KEYCODE_YEN, KeyEvent.KEYCODE_RO)
        return hasKeys.size == 2 && (hasKeys[0] || hasKeys[1])
    }

    /**
     * [mode] と（AUTOの場合のみ）実際のキー入力元デバイス [device] から、
     * このキー入力をJIS配列として扱うべきかを解決する。
     */
    fun resolveJisLayout(mode: KeyboardLayoutMode, device: InputDevice?): Boolean = when (mode) {
        KeyboardLayoutMode.JIS -> true
        KeyboardLayoutMode.US -> false
        KeyboardLayoutMode.AUTO -> isJisKeyboard(device)
    }
}
