package tools.isekai.terminal.session

import tools.isekai.terminal.HostKeyChangedWarning
import tools.isekai.terminal.data.HostKeyStatus
import tools.isekai.terminal.data.KnownHostRepository
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.TimeoutCancellationException
import kotlinx.coroutines.runBlocking
import kotlinx.coroutines.withTimeout

sealed class HostKeyDecision {
    /** 信頼する。isNew=true: TOFU 初回接続。 */
    data class Trust(val isNew: Boolean) : HostKeyDecision()
    /** ホスト鍵変更を検出 → 接続拒否。 */
    data class Changed(val warning: HostKeyChangedWarning) : HostKeyDecision()
    /** タイムアウト or エラー → 接続拒否。 */
    data class Reject(val reason: String) : HostKeyDecision()
}

interface HostKeyChecker {
    /** Rust スレッドから同期で呼ばれる。DB チェック含め同期的に完結すること。 */
    fun check(host: String, port: Int, fingerprint: String): HostKeyDecision
    /** ユーザーが変更後の鍵を明示的に信頼したときに呼ぶ。 */
    fun trustUpdated(host: String, port: Int, fingerprint: String)
}

class RealHostKeyChecker(private val repo: KnownHostRepository) : HostKeyChecker {
    override fun check(host: String, port: Int, fingerprint: String): HostKeyDecision =
        runBlocking(Dispatchers.IO) {
            try {
                withTimeout(3_000) {
                    when (val status = repo.verify(host, port, fingerprint)) {
                        HostKeyStatus.Unknown -> {
                            repo.trust(host, port, "ssh-key", fingerprint)
                            HostKeyDecision.Trust(isNew = true)
                        }
                        HostKeyStatus.Trusted -> HostKeyDecision.Trust(isNew = false)
                        is HostKeyStatus.Changed -> HostKeyDecision.Changed(
                            HostKeyChangedWarning(host, port, status.oldFingerprint, fingerprint)
                        )
                    }
                }
            } catch (e: TimeoutCancellationException) {
                HostKeyDecision.Reject("Host key check timed out")
            } catch (e: Exception) {
                HostKeyDecision.Reject("Host key check error: ${e.message}")
            }
        }

    override fun trustUpdated(host: String, port: Int, fingerprint: String) {
        runBlocking(Dispatchers.IO) {
            repo.trust(host, port, "ssh-key", fingerprint)
        }
    }
}
