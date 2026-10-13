package de.bubblebuffer.bubbleplay.input

/**
 * Keyboard lock: Android consumes the Win (Meta) key itself before the app
 * sees it, yet keys pressed while it is held still arrive with META_ON. For
 * such a key the PC gets a Win press just before the key goes down and a
 * release right after it comes up, so Win+R, Win+E or Win+Shift+S work where
 * Android leaves the combination to the app. A real Meta key event (when a
 * device delivers one) is sent as usual and suppresses the synthetic one.
 */
class MetaSync {
    private val realHeld = HashSet<String>()
    private var synthetic = false

    /** A Meta key event that reached the app and was sent to the PC. */
    fun observeMeta(code: String, up: Boolean) {
        if (up) realHeld.remove(code) else realHeld.add(code)
    }

    /** Before a non-modifier key down: true when the PC needs a synthetic Win press first. */
    fun beforeDown(metaOn: Boolean): Boolean {
        if (!metaOn || synthetic || realHeld.isNotEmpty()) return false
        synthetic = true
        return true
    }

    /** After a non-modifier key up: true when the synthetic Win press must be released now. */
    fun afterUp(): Boolean {
        if (!synthetic) return false
        synthetic = false
        return true
    }

    /** Held input was dropped (release-all, focus or session loss). */
    fun reset() {
        realHeld.clear()
        synthetic = false
    }

    companion object {
        const val META = "MetaLeft"
    }
}
