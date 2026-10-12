package de.bubblebuffer.bubbleplay.input

/**
 * Routing and units for pointer wheel and touchpad scroll (ACTION_SCROLL).
 * Values match android.view.InputDevice (SOURCE_MOUSE, SOURCE_TOUCHPAD).
 */
object PointerWheel {
    /** WHEEL_DELTA: one notch. */
    const val NOTCH_UNITS = 120f
    const val SOURCE_MOUSE = 0x2002
    const val SOURCE_TOUCHPAD = 0x100008

    /** True when a sample from [source] is a pointer wheel or touchpad source. */
    fun isPointerSource(source: Int): Boolean =
        (source and SOURCE_MOUSE) == SOURCE_MOUSE || (source and SOURCE_TOUCHPAD) == SOURCE_TOUCHPAD

    /** Axis value in notches to WHEEL_DELTA units. */
    fun units(notches: Float): Float = notches * NOTCH_UNITS
}
