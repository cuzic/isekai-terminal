package tools.isekai.terminal.input

import org.json.JSONArray
import org.json.JSONException
import org.json.JSONObject

/**
 * [KeyStep] のリストを Room の TEXT 列に保存するための JSON (de)serialize。
 * 外部の JSON ライブラリを追加せず、[tools.isekai.terminal.data.PortForwardListConverter] と
 * 同じく Android 標準の `org.json` だけで完結させている。
 *
 * Room の `@TypeConverter` そのものはここでは定義しない(Room 依存を持ち込まないため)。
 * 呼び出し側(Room エンティティの TypeConverter)がこの [encode]/[decode] を委譲呼び出しする。
 */
object KeyStepJson {
    fun encode(steps: List<KeyStep>): String {
        val arr = JSONArray()
        for (step in steps) arr.put(encodeStep(step))
        return arr.toString()
    }

    /** 1つの[KeyStep]を[JSONObject]へ変換する。[tools.isekai.terminal.pack.PackParamValuesJson]が
     *  パラメータ値(単一のKeyStep)のJSON化にもこれを再利用する(2箇所目の変換ロジックを作らない)。 */
    internal fun encodeStep(step: KeyStep): JSONObject {
        val o = JSONObject()
        when (step) {
            is KeyStep.CtrlChar -> {
                o.put("type", "ctrlChar")
                o.put("char", step.char.toString())
            }
            is KeyStep.Text -> {
                o.put("type", "text")
                o.put("text", step.text)
            }
            is KeyStep.Special -> {
                o.put("type", "special")
                o.put("keyCode", step.keyCode)
            }
            is KeyStep.PlaceholderRef -> {
                o.put("type", "placeholderRef")
                o.put("name", step.name)
            }
        }
        return o
    }

    /**
     * JSON文字列から [KeyStep] のリストを復元する。壊れたJSON全体は空リストにフォールバックする。
     * 個々の要素が未知の `type`・復元不能な値(未知の`keyCode`・空文字の`name`等)を持つ場合、
     * その要素だけをスキップし、残りは復元する(1つの壊れたstepでsequence全体を破棄しない)。
     */
    fun decode(json: String): List<KeyStep> {
        if (json.isBlank()) return emptyList()
        val arr = try {
            JSONArray(json)
        } catch (_: JSONException) {
            return emptyList()
        }
        val result = mutableListOf<KeyStep>()
        for (i in 0 until arr.length()) {
            val o = arr.optJSONObject(i) ?: continue
            val step = decodeStep(o)
            if (step != null) result.add(step)
        }
        return result
    }

    /** [encodeStep]の逆変換。復元不能なら`null`。パック機構([tools.isekai.terminal.pack.PackParamValuesJson])
     *  からも再利用する。 */
    internal fun decodeStep(o: JSONObject): KeyStep? = when (o.optString("type")) {
        "ctrlChar" -> {
            val s = o.optString("char", "")
            if (s.length == 1) KeyStep.CtrlChar(s[0]) else null
        }
        // 空文字のtextは何もバイトを出力しない無意味なstepなので、壊れたstepと同様スキップする
        // (codexレビュー指摘: {"type":"text"}のようなtext欠落JSONをText("")として復元しない)。
        "text" -> o.optString("text", "").takeIf { it.isNotEmpty() }?.let { KeyStep.Text(it) }
        "special" -> {
            val keyCode = o.optInt("keyCode", -1)
            // 未知/復元不能な keyCode は TerminalKeyEncoder が null を返すことを簡易バリデーションに使う。
            if (keyCode != -1 && TerminalKeyEncoder.specialKeyBytes(keyCode, applicationCursorMode = false) != null) {
                KeyStep.Special(keyCode)
            } else {
                null
            }
        }
        "placeholderRef" -> {
            val name = o.optString("name", "")
            if (name.isNotEmpty()) KeyStep.PlaceholderRef(name) else null
        }
        else -> null
    }
}
