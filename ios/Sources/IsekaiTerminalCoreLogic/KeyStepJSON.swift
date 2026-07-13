import Foundation

/// [KeyStep]のリストをGRDBのTEXT列に保存するためのJSON (de)serialize。
/// Android版`KeyStepJson`(Room TypeConverter用途)と対称。`JSONSerialization`(Foundation標準)
/// だけで完結させ、`Codable`の配列デコードのように1要素の失敗で全体を諦めることがないよう、
/// 手動で1要素ずつ復元してスキップする。
public enum KeyStepJSON {
    public static func encode(_ steps: [KeyStep]) -> String {
        let array = steps.map(encodeStep)
        guard
            let data = try? JSONSerialization.data(withJSONObject: array, options: []),
            let json = String(data: data, encoding: .utf8)
        else { return "[]" }
        return json
    }

    /// 1つの[KeyStep]を辞書表現へ変換する。[PackParamValuesJSON]がパラメータ値
    /// (単一のKeyStep)のJSON化にもこれを再利用する(2箇所目の変換ロジックを作らない)。
    static func encodeStep(_ step: KeyStep) -> [String: Any] {
        switch step {
        case .ctrlChar(let c):
            return ["type": "ctrlChar", "char": String(c)]
        case .text(let t):
            return ["type": "text", "text": t]
        case .special(let key):
            var dict: [String: Any] = ["type": "special", "key": key.persistenceKey]
            if case .functionKey(let n) = key { dict["number"] = n }
            return dict
        case .placeholderRef(let name):
            return ["type": "placeholderRef", "name": name]
        }
    }

    /// JSON文字列から[KeyStep]のリストを復元する。壊れたJSON全体は空リストにフォールバックする。
    /// 個々の要素が未知の`type`・復元不能な値(未知のspecialキー名・空文字の`name`等)を持つ場合、
    /// その要素だけをスキップし、残りは復元する(1つの壊れたstepでsequence全体を破棄しない)。
    public static func decode(_ json: String) -> [KeyStep] {
        guard !json.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty else { return [] }
        guard let data = json.data(using: .utf8) else { return [] }
        guard let array = (try? JSONSerialization.jsonObject(with: data, options: [])) as? [[String: Any]] else {
            return []
        }
        return array.compactMap(decodeStep)
    }

    /// [encodeStep]の逆変換。復元不能なら`nil`。[PackParamValuesJSON]からも再利用する。
    static func decodeStep(_ dict: [String: Any]) -> KeyStep? {
        guard let type = dict["type"] as? String else { return nil }
        switch type {
        case "ctrlChar":
            guard let s = dict["char"] as? String, s.count == 1, let c = s.first else { return nil }
            return .ctrlChar(c)
        case "text":
            // 空文字のtextは何もバイトを出力しない無意味なstepなので、壊れたstepと同様スキップする
            // (Android版`KeyStepJson`と同じ方針、codexレビュー指摘を踏襲)。
            guard let t = dict["text"] as? String, !t.isEmpty else { return nil }
            return .text(t)
        case "special":
            guard let keyName = dict["key"] as? String else { return nil }
            let number = dict["number"] as? Int
            guard let key = TerminalKeyMapper.SpecialKey.from(persistenceKey: keyName, functionKeyNumber: number) else {
                return nil
            }
            return .special(key)
        case "placeholderRef":
            guard let name = dict["name"] as? String, !name.isEmpty else { return nil }
            return .placeholderRef(name)
        default:
            return nil
        }
    }
}
