package tools.isekai.terminal.session

import java.io.InputStream
import kotlinx.coroutines.flow.Flow
import kotlinx.coroutines.flow.first
import kotlinx.coroutines.withTimeoutOrNull
import tools.isekai.terminal.TrzszUiState

/**
 * AND-M1a: trzszアップロードのチャンク送出ループ(Kotlin側のフロー制御)。
 *
 * 以前はContentResolverから64KiB単位で読める限り即座に`trzszSendChunk`を呼び続けて
 * おり、Rust側`SessionCmd`チャネル(容量64、`try_send`)を溢れさせるとチャンク
 * (最悪`is_last`)がサイレントに欠落しうるうえ、ユーザーがキャンセルしても最後まで
 * 読み続けていた。
 *
 * ここでは「どこまでリモートに受理されたか」を判断せず、Rust(trzsz FSM)が報告する
 * ack済みバイト数([TrzszUiState.InProgress.transferred]、upload時はリモートの`SUCC`
 * 応答でのみ進む)をそのまま使い、送信先行量を[MAX_IN_FLIGHT_BYTES]以下に抑える
 * だけにする。転送が終了/消滅([TrzszUiState.Done]・`null`)したら読み出しを止める。
 *
 * 根本的には、フロー制御の判断(bounded+awaitのAPI、または「次チャンク要求」
 * コールバック)をRust側に置くべき(AND-M1b、Rust側新API要)。
 */
internal object TrzszUploadPump {
    const val CHUNK_SIZE = 64 * 1024

    /** ack待ちで先行送信してよい最大バイト数。Rust側`SessionCmd`チャネル容量(64)より
     *  十分小さいチャンク数(4)に収まるようにする。 */
    const val MAX_IN_FLIGHT_BYTES = 4L * CHUNK_SIZE

    /** ackが一切進まない場合に諦めるまでの時間。Rust側trzsz FSMの転送タイムアウト
     *  (`TRANSFER_TIMEOUT`=120秒、タイムアウトすると`Done`になる)より長くし、通常は
     *  Rust側の判断が先に効くようにする(このタイムアウトはセッションが既に破棄されて
     *  状態が二度と更新されない場合の保険)。 */
    const val ACK_WAIT_TIMEOUT_MS = 150_000L

    enum class Result {
        /** 最終チャンク(`isLast=true`)まで送出した。 */
        COMPLETED,

        /** 転送が終了/中断された(Done・状態消滅)ため途中で止めた。 */
        STOPPED,

        /** ackが[ACK_WAIT_TIMEOUT_MS]以上進まなかった。呼び出し元はキャンセルを送るべき。 */
        TIMED_OUT,
    }

    suspend fun pump(
        input: InputStream,
        trzszState: Flow<TrzszUiState?>,
        sendChunk: (data: ByteArray, isLast: Boolean) -> Unit,
        chunkSize: Int = CHUNK_SIZE,
        maxInFlightBytes: Long = MAX_IN_FLIGHT_BYTES,
        ackWaitTimeoutMs: Long = ACK_WAIT_TIMEOUT_MS,
    ): Result {
        val buf = ByteArray(chunkSize)
        var sent = 0L
        // 最終チャンクに`isLast=true`を付けるため、1チャンク遅らせて送る。
        var pending: ByteArray? = null
        while (true) {
            val n = input.read(buf)
            val isEof = n == -1
            val chunk = if (isEof) (pending ?: ByteArray(0)) else pending
            if (chunk != null) {
                when (awaitWindow(trzszState, sent + chunk.size, maxInFlightBytes, ackWaitTimeoutMs)) {
                    WindowOutcome.OPEN -> {}
                    WindowOutcome.TRANSFER_ENDED -> return Result.STOPPED
                    WindowOutcome.TIMED_OUT -> return Result.TIMED_OUT
                }
                sendChunk(chunk, isEof)
                sent += chunk.size
            }
            if (isEof) return Result.COMPLETED
            pending = buf.copyOf(n)
        }
    }

    private enum class WindowOutcome { OPEN, TRANSFER_ENDED, TIMED_OUT }

    private suspend fun awaitWindow(
        trzszState: Flow<TrzszUiState?>,
        sentAfterThisChunk: Long,
        maxInFlightBytes: Long,
        timeoutMs: Long,
    ): WindowOutcome {
        // `first`の結果自体が`null`(=転送状態の消滅)になりうるため、タイムアウトの`null`と
        // 区別できるよう1要素リストで包む。
        val wrapped: List<TrzszUiState?> = withTimeoutOrNull(timeoutMs) {
            listOf(trzszState.first { st ->
                when (st) {
                    null, is TrzszUiState.Done -> true
                    // accept直後(リモートのSIZE ack前)はまだWaitingUserのまま。ack済み0として扱う。
                    is TrzszUiState.WaitingUser -> sentAfterThisChunk <= maxInFlightBytes
                    is TrzszUiState.InProgress -> sentAfterThisChunk - st.transferred.toLong() <= maxInFlightBytes
                }
            })
        } ?: return WindowOutcome.TIMED_OUT
        return when (wrapped.single()) {
            null, is TrzszUiState.Done -> WindowOutcome.TRANSFER_ENDED
            else -> WindowOutcome.OPEN
        }
    }
}
