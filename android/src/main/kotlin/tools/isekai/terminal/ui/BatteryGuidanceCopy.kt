package tools.isekai.terminal.ui

/**
 * 項目2(OEMバッテリー最適化への案内UI)の表示文言。`Build.MANUFACTURER`から
 * [forManufacturer]で解決する。
 *
 * OEM別Activity(Xiaomiの自動起動管理画面等)への直接遷移は実装しない(非公開の
 * コンポーネント名に依存するためROMのバージョンごとに壊れやすく、検証できる実機も
 * 無いため)。ここでやるのは文言だけの出し分けであり、実際の遷移先は常に標準API
 * (`Settings.ACTION_IGNORE_BATTERY_OPTIMIZATION_SETTINGS`、`BatteryOptimization.kt`
 * 参照)の1本に統一する——OEM別ヒント文言は、その標準設定画面の中でユーザーが
 * 自力で辿るべきメニュー名を示すだけの補足情報にとどめる。
 *
 * `Build.MANUFACTURER`を直接読まず`String`引数として受け取る設計にしているのは、
 * Robolectric無しの素のJUnitテストから検証できるようにするため
 * ([tools.isekai.terminal.BatteryGuidanceCopyTest]参照)。
 */
object BatteryGuidanceCopy {
    data class Copy(
        val title: String,
        val body: String,
        /** そのOEMの設定アプリ内でのメニュー名の補足(既知でない場合は`null`)。 */
        val oemHint: String?,
    )

    private const val TITLE = "バックグラウンド動作の最適化"
    private const val BODY =
        "端末の省電力機能により、バックグラウンドでSSHセッションを保持できず、" +
            "接続が切れることがあります。次の画面でisekai-terminalをバッテリー最適化の" +
            "対象から外すことをお勧めします(切断されても自動的に再接続を試みますが、" +
            "外しておくとそもそも切断されにくくなります)。"

    fun forManufacturer(manufacturer: String): Copy {
        val normalized = manufacturer.lowercase()
        val oemHint = when {
            normalized.contains("xiaomi") ->
                "MIUI/HyperOSの場合: 設定 → アプリ → isekai-terminal → バッテリー → 「制限なし」"
            normalized.contains("oppo") || normalized.contains("realme") ->
                "ColorOSの場合: 設定 → バッテリー使用状況 → isekai-terminal → バックグラウンド実行を許可"
            normalized.contains("vivo") ->
                "OriginOS/FuntouchOSの場合: 設定 → バッテリー → 高消費電力アプリの管理 → isekai-terminalを許可"
            normalized.contains("huawei") || normalized.contains("honor") ->
                "EMUI/HarmonyOSの場合: 設定 → バッテリー → アプリ起動の管理 → isekai-terminalを手動管理に切替"
            normalized.contains("samsung") ->
                "One UIの場合: 設定 → バッテリー → バックグラウンド使用の制限 → isekai-terminalを対象外に"
            else -> null
        }
        return Copy(title = TITLE, body = BODY, oemHint = oemHint)
    }
}
