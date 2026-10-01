package de.bubblebuffer.bubbleplay.input

import kotlin.math.pow

/**
 * Trackpad pointer speed: a level from [MIN] to [MAX] maps to the gain applied
 * to finger motion (screen px -> cursor px at the current zoom). [DEFAULT]
 * is the gain the app always used (1.6); each step is 25 % faster or slower,
 * so the range is about 0.66 to 3.9.
 */
object PointerSpeed {
    const val MIN = 1
    const val MAX = 9
    const val DEFAULT = 5
    const val DEFAULT_GAIN = 1.6f
    private const val STEP = 1.25f

    fun clamp(level: Int): Int = level.coerceIn(MIN, MAX)

    fun gain(level: Int): Float = DEFAULT_GAIN * STEP.pow(clamp(level) - DEFAULT)
}
