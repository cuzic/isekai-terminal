import Foundation

/// Y-P1(#2): `presentForm`パネルの送信バイト列(1行JSON + 改行)を、フォーム入力値から
/// 組み立てる純関数。`AI_INTEGRATION_DESIGN.md` §6.2の方針通り、Rust側へ返す専用チャネルは
/// 持たずPTYへの通常のstdin文字列書き込みで返す(Android版`TerminalSession.submitAiPanelForm`
/// と対称、`ADR_IOS_PARITY_IMPLEMENTATION.md` §3.2)。
///
/// **信頼境界**: 送信内容はあくまで表示専用パネルへのユーザー入力であり、
/// Rust/Swiftどちらの側でも実行・評価はしない。
public enum AiPanelFormSubmission {
    /// `orderedValues`の順序をそのままJSONオブジェクトのキー順として出力する
    /// (呼び出し元順序に対して**決定的**——同じ順序の入力からは常に同じバイト列を返す)。
    /// Android版`org.json.JSONObject(map).toString()`の順序(渡したMapのイテレーション順に
    /// 従う仕様外の挙動)と一致することは意図しておらず、検証もしない——それは
    /// パリティ・プロパティではない。
    public static func bytes(orderedValues: [(key: String, value: String)]) -> Data {
        var json = "{"
        for (index, pair) in orderedValues.enumerated() {
            if index > 0 { json += "," }
            json += "\"\(escape(pair.key))\":\"\(escape(pair.value))\""
        }
        json += "}\n"
        return Data(json.utf8)
    }

    /// JSON文字列リテラル用の最小限のエスケープ。`"`/`\`/改行系制御文字のみを対象にし、
    /// 非ASCII文字(日本語等)は`\uXXXX`化せずUTF-8のまま素通しする(JSON仕様上必須ではない)。
    private static func escape(_ value: String) -> String {
        var result = ""
        result.reserveCapacity(value.count)
        for scalar in value.unicodeScalars {
            switch scalar {
            case "\"": result += "\\\""
            case "\\": result += "\\\\"
            case "\n": result += "\\n"
            case "\r": result += "\\r"
            case "\t": result += "\\t"
            default:
                if scalar.value < 0x20 {
                    result += String(format: "\\u%04x", scalar.value)
                } else {
                    result.unicodeScalars.append(scalar)
                }
            }
        }
        return result
    }
}
