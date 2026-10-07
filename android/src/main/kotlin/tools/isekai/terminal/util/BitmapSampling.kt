package tools.isekai.terminal.util

/**
 * `BitmapFactory.Options.inSampleSize`の計算(AND-L1/AND-L9)。デコード後のピクセル数が
 * [maxPixels]以下になる最小の2の冪を返す(`BitmapFactory`は2の冪以外を切り捨てて扱う)。
 * 幅/高さが不明(0以下)なら1を返す。
 */
object BitmapSampling {
    fun inSampleSizeFor(width: Int, height: Int, maxPixels: Long): Int {
        if (width <= 0 || height <= 0 || maxPixels <= 0) return 1
        var sample = 1
        while ((width.toLong() / sample) * (height.toLong() / sample) > maxPixels && sample < (1 shl 30)) {
            sample *= 2
        }
        return sample
    }
}
