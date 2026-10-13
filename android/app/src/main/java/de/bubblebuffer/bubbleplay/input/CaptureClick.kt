package de.bubblebuffer.bubbleplay.input

/**
 * Game mode: the click that captures the mouse is the browser's swallowed
 * capture click, so it never reaches the PC. The buttons held at capture stay
 * hidden from the remote until they are released; pressing one again later is
 * an ordinary click.
 */
class CaptureClick {
    private var swallowed = 0

    /** Pointer capture was requested by a click with these buttons down. */
    fun captureRequested(mask: Int) { swallowed = mask }

    /** The button mask to send for a captured sample whose buttons are [mask]. */
    fun filter(mask: Int): Int {
        swallowed = swallowed and mask
        return mask and swallowed.inv()
    }

    fun reset() { swallowed = 0 }
}
