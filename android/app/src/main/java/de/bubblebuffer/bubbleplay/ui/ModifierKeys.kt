package de.bubblebuffer.bubbleplay.ui

/**
 * Soft-keyboard modifier state: Ctrl, Alt, Shift and Win are off, latched (one-shot, sent
 * with the next key by [LatchedKeySender]) or locked (held down on the PC until unlocked).
 * [send] sends one key press or release to the PC.
 */
internal class ModifierKeys(private val send: (code: String, up: Boolean) -> Boolean) {
    enum class State { OFF, LATCHED, LOCKED }

    /** One-shot modifiers waiting for the next key; consumed by [LatchedKeySender]. */
    val latched = LinkedHashSet<String>()
    /** Modifiers held down on the PC. */
    val locked = LinkedHashSet<String>()
    /** Modifiers a currently pressed key took along; owned by [LatchedKeySender]. */
    val active = HashMap<String, Set<String>>()

    val any: Boolean get() = latched.isNotEmpty() || locked.isNotEmpty()

    fun state(code: String): State = when (code) {
        in locked -> State.LOCKED
        in latched -> State.LATCHED
        else -> State.OFF
    }

    /** Tap: off -> latched, latched -> locked (second tap), locked -> off. */
    fun tap(code: String) = when (state(code)) {
        State.OFF -> { latched.add(code); Unit }
        State.LATCHED -> lock(code)
        State.LOCKED -> unlock(code)
    }

    /** Long press: off or latched -> locked, locked -> off. */
    fun longPress(code: String) = if (code in locked) unlock(code) else lock(code)

    private fun lock(code: String) {
        latched.remove(code)
        locked.add(code)
        send(code, false)
    }

    private fun unlock(code: String) {
        locked.remove(code)
        send(code, true)
    }

    /**
     * A preset chord: exact, so latches are ignored and kept, and locks are neither sent again
     * nor released (a lock is left held across the chord).
     */
    fun chord(mods: List<String>, key: String) {
        val own = mods.filter { it !in locked }
        own.forEach { send(it, false) }
        send(key, false)
        send(key, true)
        own.asReversed().forEach { send(it, true) }
    }

    /**
     * Forget every latch and lock. With [release], held locks are released on the PC first;
     * without it the PC already dropped its held input.
     */
    fun releaseAll(release: Boolean) {
        if (release) locked.forEach { send(it, true) }
        latched.clear(); locked.clear(); active.clear()
    }
}
