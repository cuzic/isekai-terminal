package tools.isekai.terminal.session

import tools.isekai.terminal.HostKeyChangedWarning
import tools.isekai.terminal.NewHostKeyPrompt
import tools.isekai.terminal.data.HostKeyStatus
import tools.isekai.terminal.data.KnownHostRepository
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.TimeoutCancellationException
import kotlinx.coroutines.runBlocking
import kotlinx.coroutines.withTimeout

sealed class HostKeyDecision {
    /** 信頼する。isNew=true: TOFU 初回接続(確認不要設定、または確認済み再試行)。 */
    data class Trust(val isNew: Boolean) : HostKeyDecision()
    /** ホスト鍵変更を検出 → 接続拒否。 */
    data class Changed(val warning: HostKeyChangedWarning) : HostKeyDecision()
    /** 初回接続(Unknown)で、ユーザーの明示確認が必要 → 一旦接続拒否し確認ダイアログを出す。 */
    data class Unconfirmed(val prompt: NewHostKeyPrompt) : HostKeyDecision()
    /** タイムアウト or エラー → 接続拒否。 */
    data class Reject(val reason: String) : HostKeyDecision()
}

interface HostKeyChecker {
    /** Rust スレッドから同期で呼ばれる。DB チェック含め同期的に完結すること
     *  (ユーザー入力待ちでブロックしてはいけない — 初回接続の確認待ちは
     *  [HostKeyDecision.Unconfirmed] を返して一旦接続を拒否し、
     *  ユーザーが確認した後の再接続に委ねる)。 */
    fun check(host: String, port: Int, fingerprint: String): HostKeyDecision
    /** ユーザーが変更後の鍵、または初回接続の鍵を明示的に信頼したときに呼ぶ。 */
    fun trustUpdated(host: String, port: Int, fingerprint: String)
}

class RealHostKeyChecker(
    private val repo: KnownHostRepository,
    /** 初回接続(Unknown host key)を確認ダイアログ無しで自動信頼するか(既定: false=確認あり)。
     *  呼び出しの都度評価する(設定変更を即座に反映するため)。 */
    private val autoTrustNewHostKeys: () -> Boolean = { false },
) : HostKeyChecker {
    override fun check(host: String, port: Int, fingerprint: String): HostKeyDecision =
        runBlocking(Dispatchers.IO) {
            try {
                withTimeout(3_000) {
                    when (val status = repo.verify(host, port, fingerprint)) {
                        HostKeyStatus.Unknown -> {
                            if (autoTrustNewHostKeys()) {
                                repo.trust(host, port, "ssh-key", fingerprint)
                                HostKeyDecision.Trust(isNew = true)
                            } else {
                                HostKeyDecision.Unconfirmed(NewHostKeyPrompt(host, port, fingerprint))
                            }
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
