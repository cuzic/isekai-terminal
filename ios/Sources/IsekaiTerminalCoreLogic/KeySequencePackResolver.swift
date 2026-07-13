import Foundation

/// パック内の1打鍵列を、プレースホルダーを解決した具体的な[steps]で表現したもの。
/// Android版`ResolvedPackSequence`と対称。
public struct ResolvedPackSequence: Equatable {
    public let packId: String
    public let label: String
    public let steps: [KeyStep]
}

/// 「ライブバインディング」方式でのプレースホルダー解決。パック定義はテンプレートのまま保持し、
/// 送信/表示のたびにこの関数でinstallationの`paramValues`へ都度解決する(有効化時に打鍵列を
/// 複製する「マテリアライズ方式」は不採用。ユーザーがprefixキーを後から変更した場合に
/// 1箇所の変更で全ボタンへ反映されるようにするため)。Android版`KeySequencePackResolver`と対称。
///
/// バージョン互換ルール: 未知のplaceholder名(installationにもパック定義にも無い)は
/// その[KeyStep.placeholderRef]をそのまま残す(=送信時に何も出力されない安全側の挙動、
/// [KeySequenceCommands.toBytes]参照)。廃止されたplaceholderを含む古い`paramValues`のキーは
/// 単に参照されず無視される。
public enum KeySequencePackResolver {
    public static func resolve(pack: KeySequencePack, paramValues: [String: KeyStep]) -> [ResolvedPackSequence] {
        pack.sequences.map { template in
            ResolvedPackSequence(
                packId: pack.id,
                label: template.label,
                steps: template.steps.map { resolveStep(pack: pack, paramValues: paramValues, step: $0) }
            )
        }
    }

    private static func resolveStep(pack: KeySequencePack, paramValues: [String: KeyStep], step: KeyStep) -> KeyStep {
        guard case .placeholderRef(let name) = step else { return step }
        // installationに値があればそれを優先、無ければパック定義のdefaultにフォールバックする。
        if let value = paramValues[name] { return value }
        if let param = pack.params.first(where: { $0.name == name }) { return param.defaultStep }
        return step
    }
}
