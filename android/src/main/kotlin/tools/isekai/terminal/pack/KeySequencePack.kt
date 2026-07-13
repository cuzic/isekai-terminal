package tools.isekai.terminal.pack

import tools.isekai.terminal.input.KeyStep

/**
 * 打鍵列セット(パック)のプレースホルダーパラメータ定義。例: tmuxパックの"prefix"
 * (ラベル「tmuxプレフィックスキー」、既定値 Ctrl+B)。
 */
data class PackParam(val name: String, val label: String, val default: KeyStep)

/** パック内の1打鍵列テンプレート。[steps]は[KeyStep.PlaceholderRef]を含みうる。 */
data class PackSequenceTemplate(val label: String, val steps: List<KeyStep>)

/**
 * 打鍵列セット(パック)の静的定義。DB行ではなくアプリに同梱するコンテンツ。
 * このデータモデル自体はtmux等の特定ソフトウェアの知識を持たない汎用エンジン
 * (エンジンが知っているのは「プレースホルダーを持つ打鍵列テンプレートの集合」のみ)。
 *
 * [version]はライブバインディング解決ロジックの分岐には使わない(表示・デバッグ用の記録に
 * 留める)。破壊的変更が必要な場合は新しい[id]を発行する方針(#18タスク参照)。
 */
data class KeySequencePack(
    val id: String,
    val version: Int,
    val name: String,
    val params: List<PackParam>,
    val sequences: List<PackSequenceTemplate>,
)

/** アプリに同梱するパック一覧。MVPではtmuxパック1つのみ。 */
object KeySequencePacks {
    val TMUX = KeySequencePack(
        id = "tmux",
        version = 1,
        name = "tmux",
        params = listOf(
            PackParam(name = "prefix", label = "tmuxプレフィックスキー", default = KeyStep.CtrlChar('b')),
        ),
        sequences = listOf(
            PackSequenceTemplate("新規ウィンドウ", listOf(KeyStep.PlaceholderRef("prefix"), KeyStep.Text("c"))),
            // tmuxの`%`/`"`は「縦分割」「横分割」という呼び方だと分割線の向きか配置の向きかで
            // 解釈が割れるため(2回目のcodexレビュー指摘)、明確な「左右」「上下」表記にする。
            // `%`(split-window -h)は左右に並べる(パネル境界線は縦線)、
            // `"`(split-window -v)は上下に並べる(パネル境界線は横線)、というtmuxの実際の挙動と一致。
            PackSequenceTemplate("ペイン分割(左右)", listOf(KeyStep.PlaceholderRef("prefix"), KeyStep.Text("%"))),
            PackSequenceTemplate("ペイン分割(上下)", listOf(KeyStep.PlaceholderRef("prefix"), KeyStep.Text("\""))),
            PackSequenceTemplate("次のペイン", listOf(KeyStep.PlaceholderRef("prefix"), KeyStep.Text("o"))),
            PackSequenceTemplate("デタッチ", listOf(KeyStep.PlaceholderRef("prefix"), KeyStep.Text("d"))),
        ),
    )

    val ALL: List<KeySequencePack> = listOf(TMUX)

    fun findById(id: String): KeySequencePack? = ALL.firstOrNull { it.id == id }
}
