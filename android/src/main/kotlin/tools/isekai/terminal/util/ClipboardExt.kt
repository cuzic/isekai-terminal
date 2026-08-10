package tools.isekai.terminal.util

import android.content.ClipData
import android.content.ClipboardManager
import android.content.Context

/**
 * 単純なプレーンテキストをクリップボードへ書き込む。`getSystemService(CLIPBOARD_SERVICE)
 * as ClipboardManager` + `setPrimaryClip(ClipData.newPlainText(...))` という定型処理が
 * 複数箇所(選択範囲コピー・ログコピー・鍵の公開鍵コピー等)で重複していたため抽出した。
 *
 * リモート(OSC 52)由来のHTML/PNGクリップボード書き込み([TerminalTabsViewModel]の
 * `writeToClipboard`)はMIMEタイプごとに異なる[ClipData]構築が必要でこの単純な形に
 * 収まらないため、対象外(そちらは今まで通り直接[ClipData]を組み立てる)。
 */
fun Context.copyToClipboard(label: String, text: String) {
    val cm = getSystemService(Context.CLIPBOARD_SERVICE) as ClipboardManager
    cm.setPrimaryClip(ClipData.newPlainText(label, text))
}
