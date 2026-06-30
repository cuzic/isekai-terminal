package com.example.imespike.session

import android.content.Context
import com.example.imespike.HostKeyChangedWarning
import com.example.imespike.data.HostKeyStatus
import com.example.imespike.data.Repositories
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.runBlocking

sealed class HostKeyDecision {
    /** 信頼する。isNew=true: TOFU 初回接続。 */
    data class Trust(val isNew: Boolean) : HostKeyDecision()
    /** ホスト鍵変更を検出 → 接続拒否。 */
    data class Changed(val warning: HostKeyChangedWarning) : HostKeyDecision()
}

interface HostKeyChecker {
    /** Rust スレッドから同期で呼ばれる。DB チェック含め同期的に完結すること。 */
    fun check(host: String, port: Int, fingerprint: String): HostKeyDecision
    /** ユーザーが変更後の鍵を明示的に信頼したときに呼ぶ。 */
    fun trustUpdated(host: String, port: Int, fingerprint: String)
}

class RealHostKeyChecker(private val context: Context) : HostKeyChecker {
    override fun check(host: String, port: Int, fingerprint: String): HostKeyDecision =
        runBlocking(Dispatchers.IO) {
            Repositories.init(context)
            when (val status = Repositories.knownHosts.verify(host, port, fingerprint)) {
                HostKeyStatus.Unknown -> {
                    Repositories.knownHosts.trust(host, port, "ssh-key", fingerprint)
                    HostKeyDecision.Trust(isNew = true)
                }
                HostKeyStatus.Trusted -> HostKeyDecision.Trust(isNew = false)
                is HostKeyStatus.Changed -> HostKeyDecision.Changed(
                    HostKeyChangedWarning(host, port, status.oldFingerprint, fingerprint)
                )
            }
        }

    override fun trustUpdated(host: String, port: Int, fingerprint: String) {
        runBlocking(Dispatchers.IO) {
            Repositories.knownHosts.trust(host, port, "ssh-key", fingerprint)
        }
    }
}
