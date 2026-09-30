package tools.isekai.terminal.util

import org.junit.Assert.assertEquals
import org.junit.Assert.assertTrue
import org.junit.Test

/** AND-L1/AND-L9: 縮小デコードの`inSampleSize`計算。 */
class BitmapSamplingTest {
    @Test
    fun smallImage_isNotDownsampled() {
        assertEquals(1, BitmapSampling.inSampleSizeFor(1920, 1080, 4_000_000))
    }

    @Test
    fun hugeImage_isDownsampledUnderTheLimitByPowerOfTwo() {
        val sample = BitmapSampling.inSampleSizeFor(8000, 5000, 4_000_000)
        assertEquals(4, sample)
        assertTrue((8000L / sample) * (5000L / sample) <= 4_000_000)
    }

    @Test
    fun unknownBounds_fallBackToOne() {
        assertEquals(1, BitmapSampling.inSampleSizeFor(0, 100, 4_000_000))
        assertEquals(1, BitmapSampling.inSampleSizeFor(-1, -1, 4_000_000))
    }
}
