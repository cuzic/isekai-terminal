package tools.isekai.terminal.pack

import org.json.JSONException
import org.json.JSONObject
import tools.isekai.terminal.input.KeyStep
import tools.isekai.terminal.input.KeyStepJson

/**
 * パックインストールの`paramValues`(パラメータ名→[KeyStep]のマップ、例: `"prefix" -> CtrlChar('b')`)
 * のJSON (de)serialize。[KeyStepJson]の1ステップ変換をそのまま再利用し、ステップの
 * バイト変換ロジックを重複させない。
 */
object PackParamValuesJson {
    fun encode(values: Map<String, KeyStep>): String {
        val o = JSONObject()
        for ((name, step) in values) o.put(name, KeyStepJson.encodeStep(step))
        return o.toString()
    }

    /** 壊れたJSONは空マップにフォールバックする。個々の値が復元不能な場合はそのキーだけ
     *  スキップする(呼び出し側はパック定義の`default`にフォールバックできる)。 */
    fun decode(json: String): Map<String, KeyStep> {
        if (json.isBlank()) return emptyMap()
        val o = try {
            JSONObject(json)
        } catch (_: JSONException) {
            return emptyMap()
        }
        val result = mutableMapOf<String, KeyStep>()
        for (name in o.keys()) {
            val stepObj = o.optJSONObject(name) ?: continue
            KeyStepJson.decodeStep(stepObj)?.let { result[name] = it }
        }
        return result
    }
}
