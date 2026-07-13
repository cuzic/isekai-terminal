package tools.isekai.terminal.session

import tools.isekai.terminal.data.AuthType

/**
 * プロファイルの認証パラメータ検証ロジック。
 * Android・UniFFI 依存なし。純粋関数。
 */
sealed class AuthValidation {
    data class Password(val value: String) : AuthValidation()
    data class PublicKey(val keyId: Long) : AuthValidation()
    data class Error(val statusMsg: String) : AuthValidation()
}

object AuthValidator {
    /** 生の文字列(`ConnectionProfile.authType`等、DB由来)を検証する後方互換版。
     *  [AuthType]に変換できない未知の文字列は、その場でErrorにする。 */
    fun validate(authType: String, password: String?, keyId: Long?): AuthValidation {
        val typed = AuthType.fromRaw(authType)
            ?: return AuthValidation.Error("未知の認証タイプ: $authType")
        return validate(typed, password, keyId)
    }

    fun validate(authType: AuthType, password: String?, keyId: Long?): AuthValidation =
        when (authType) {
            AuthType.PASSWORD ->
                if (password.isNullOrEmpty()) AuthValidation.Error("パスワードが必要です")
                else AuthValidation.Password(password)
            AuthType.KEY ->
                if (keyId == null) AuthValidation.Error("鍵IDが未設定です")
                else AuthValidation.PublicKey(keyId)
        }
}
