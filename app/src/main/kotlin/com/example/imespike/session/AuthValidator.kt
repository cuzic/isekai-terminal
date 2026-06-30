package com.example.imespike.session

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
    fun validate(authType: String, password: String?, keyId: Long?): AuthValidation =
        when (authType) {
            "password" ->
                if (password.isNullOrEmpty()) AuthValidation.Error("パスワードが必要です")
                else AuthValidation.Password(password)
            "key" ->
                if (keyId == null) AuthValidation.Error("鍵IDが未設定です")
                else AuthValidation.PublicKey(keyId)
            else ->
                AuthValidation.Error("未知の認証タイプ: $authType")
        }
}
