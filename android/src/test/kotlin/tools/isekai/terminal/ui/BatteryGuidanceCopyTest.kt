package tools.isekai.terminal.ui

import org.junit.Assert.assertEquals
import org.junit.Assert.assertNotNull
import org.junit.Assert.assertNull
import org.junit.Test

class BatteryGuidanceCopyTest {

    @Test
    fun unknownManufacturer_hasNoOemHint() {
        val copy = BatteryGuidanceCopy.forManufacturer("Google")
        assertNull(copy.oemHint)
    }

    @Test
    fun xiaomi_getsMiuiSpecificHint() {
        val copy = BatteryGuidanceCopy.forManufacturer("Xiaomi")
        assertNotNull(copy.oemHint)
        assertEquals(true, copy.oemHint!!.contains("MIUI"))
    }

    @Test
    fun manufacturerMatchIsCaseInsensitive() {
        val lower = BatteryGuidanceCopy.forManufacturer("xiaomi")
        val upper = BatteryGuidanceCopy.forManufacturer("XIAOMI")
        assertEquals(lower.oemHint, upper.oemHint)
    }

    @Test
    fun oppoAndRealmeShareCopy() {
        val oppo = BatteryGuidanceCopy.forManufacturer("OPPO")
        val realme = BatteryGuidanceCopy.forManufacturer("realme")
        assertEquals(oppo.oemHint, realme.oemHint)
    }

    @Test
    fun titleAndBodyAreConstantAcrossManufacturers() {
        val a = BatteryGuidanceCopy.forManufacturer("Samsung")
        val b = BatteryGuidanceCopy.forManufacturer("Sony")
        assertEquals(a.title, b.title)
        assertEquals(a.body, b.body)
    }
}
