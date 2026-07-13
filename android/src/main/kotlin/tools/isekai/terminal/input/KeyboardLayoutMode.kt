package tools.isekai.terminal.input

/**
 * 物理/Bluetoothキーボードの配列モード。JIS配列固有キー(¥キー・ろキー)を
 * 正しくバイト列へマッピングするための設定（[KeyboardLayoutDetector]参照）。
 *
 * どのSSHホストに接続していても使う物理キーボード自体の特性であり、接続先ホストごとの
 * 設定ではないため、`ConnectionProfile`にはぶら下げず、配色テーマ・画面の保護などと同じ
 * グローバル設定(`SharedPreferences("isekai_terminal_ui")`)として`ProfileListScreen`の
 * メニューから設定する。
 */
enum class KeyboardLayoutMode {
    /** 接続中の外部キーボードの物理キー構成から自動判定する（既定）。 */
    AUTO,

    /** 常にJIS配列として扱う（自動判定が外れる場合の手動上書き）。 */
    JIS,

    /** 常にUS配列として扱う（¥/ろキーの明示マッピングを行わない）。 */
    US;

    /** UI表示用の日本語ラベル。 */
    fun label(): String = when (this) {
        AUTO -> "自動判定"
        JIS -> "JIS配列"
        US -> "US配列"
    }

    companion object {
        const val PREF_KEY = "keyboard_layout_mode"

        /** SharedPreferences に保存された文字列から復元する。不正/未設定時は [AUTO]。 */
        fun fromPrefValue(value: String?): KeyboardLayoutMode =
            value?.let { name ->
                try {
                    valueOf(name)
                } catch (_: IllegalArgumentException) {
                    null
                }
            } ?: AUTO
    }
}
