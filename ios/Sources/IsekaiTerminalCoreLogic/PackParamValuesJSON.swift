import Foundation

/// パックインストールの`paramValues`(パラメータ名→[KeyStep]のマップ、例: `"prefix" -> .ctrlChar("b")`)
/// のJSON (de)serialize。[KeyStepJSON]の1ステップ変換をそのまま再利用し、ステップの
/// バイト変換ロジックを重複させない。Android版`PackParamValuesJson`と対称。
public enum PackParamValuesJSON {
    public static func encode(_ values: [String: KeyStep]) -> String {
        var dict: [String: Any] = [:]
        for (name, step) in values { dict[name] = KeyStepJSON.encodeStep(step) }
        guard
            let data = try? JSONSerialization.data(withJSONObject: dict, options: []),
            let json = String(data: data, encoding: .utf8)
        else { return "{}" }
        return json
    }

    /// 壊れたJSONは空マップにフォールバックする。個々の値が復元不能な場合はそのキーだけ
    /// スキップする(呼び出し側はパック定義の`default`にフォールバックできる)。
    public static func decode(_ json: String) -> [String: KeyStep] {
        guard !json.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty else { return [:] }
        guard let data = json.data(using: .utf8) else { return [:] }
        guard let dict = (try? JSONSerialization.jsonObject(with: data, options: [])) as? [String: [String: Any]] else {
            return [:]
        }
        var result: [String: KeyStep] = [:]
        for (name, stepDict) in dict {
            if let step = KeyStepJSON.decodeStep(stepDict) { result[name] = step }
        }
        return result
    }
}
