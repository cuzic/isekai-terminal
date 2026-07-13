package tools.isekai.terminal.pack

import tools.isekai.terminal.input.KeyStep

/** パック内の1打鍵列を、プレースホルダーを解決した具体的な[steps]で表現したもの。 */
data class ResolvedPackSequence(val packId: String, val label: String, val steps: List<KeyStep>)

/**
 * 「ライブバインディング」方式でのプレースホルダー解決。パック定義はテンプレートのまま保持し、
 * 送信/表示のたびにこの関数でinstallationの`paramValues`へ都度解決する(#18タスクの設計:
 * 有効化時に打鍵列を複製する「マテリアライズ方式」は不採用。ユーザーがprefixキーを後から
 * 変更した場合に1箇所の変更で全ボタンへ反映されるようにするため)。
 *
 * バージョン互換ルール: 未知のplaceholder名(installationにもパック定義にも無い)は
 * その[KeyStep.PlaceholderRef]をそのまま残す(=送信時に何も出力されない安全側の挙動、
 * [tools.isekai.terminal.KeySequenceCommands.toBytes]参照)。廃止されたplaceholderを含む
 * 古い`paramValues`のキーは単に参照されず無視される。
 */
object KeySequencePackResolver {
    fun resolve(pack: KeySequencePack, paramValues: Map<String, KeyStep>): List<ResolvedPackSequence> =
        pack.sequences.map { template ->
            ResolvedPackSequence(
                packId = pack.id,
                label = template.label,
                steps = template.steps.map { step -> resolveStep(pack, paramValues, step) },
            )
        }

    private fun resolveStep(pack: KeySequencePack, paramValues: Map<String, KeyStep>, step: KeyStep): KeyStep {
        if (step !is KeyStep.PlaceholderRef) return step
        // installationに値があればそれを優先、無ければパック定義のdefaultにフォールバックする。
        return paramValues[step.name]
            ?: pack.params.firstOrNull { it.name == step.name }?.default
            ?: step
    }
}
