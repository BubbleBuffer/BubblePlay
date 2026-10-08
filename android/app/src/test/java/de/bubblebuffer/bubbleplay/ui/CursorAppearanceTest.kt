package de.bubblebuffer.bubbleplay.ui

import android.view.PointerIcon
import de.bubblebuffer.bubbleplay.Fixtures
import de.bubblebuffer.bubbleplay.protocol.CursorKind
import org.junit.Assert.assertEquals
import org.junit.Test

class CursorAppearanceTest {
    @Test
    fun androidSystemTypesMatchSharedContract() {
        val androidTypes = mapOf(
            "TYPE_ARROW" to PointerIcon.TYPE_ARROW,
            "TYPE_TEXT" to PointerIcon.TYPE_TEXT,
            "TYPE_HAND" to PointerIcon.TYPE_HAND,
            "TYPE_CROSSHAIR" to PointerIcon.TYPE_CROSSHAIR,
            "TYPE_ALL_SCROLL" to PointerIcon.TYPE_ALL_SCROLL,
            "TYPE_VERTICAL_DOUBLE_ARROW" to PointerIcon.TYPE_VERTICAL_DOUBLE_ARROW,
            "TYPE_HORIZONTAL_DOUBLE_ARROW" to PointerIcon.TYPE_HORIZONTAL_DOUBLE_ARROW,
            "TYPE_TOP_RIGHT_DIAGONAL_DOUBLE_ARROW" to PointerIcon.TYPE_TOP_RIGHT_DIAGONAL_DOUBLE_ARROW,
            "TYPE_TOP_LEFT_DIAGONAL_DOUBLE_ARROW" to PointerIcon.TYPE_TOP_LEFT_DIAGONAL_DOUBLE_ARROW,
            "TYPE_WAIT" to PointerIcon.TYPE_WAIT,
            "TYPE_NO_DROP" to PointerIcon.TYPE_NO_DROP,
            "TYPE_HELP" to PointerIcon.TYPE_HELP,
            "TYPE_NULL" to PointerIcon.TYPE_NULL,
        )
        val types = Fixtures.json("cursor-types.json").getJSONArray("types")
        for (i in 0 until types.length()) {
            val type = types.getJSONObject(i)
            assertEquals(type.getString("name"), androidTypes.getValue(type.getString("androidType")),
                CursorAppearance.systemType(CursorKind.fromWire(type.getInt("id"))))
        }
    }
}
