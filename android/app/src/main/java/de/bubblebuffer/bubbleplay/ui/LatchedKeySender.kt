package de.bubblebuffer.bubbleplay.ui

import de.bubblebuffer.bubbleplay.protocol.KeyMap

/** Shared press/release ordering for IME, hardware keys and the QuickBar. */
internal class LatchedKeySender(
    private val latches: MutableSet<String>,
    private val active: MutableMap<String, Set<String>>,
    private val send: (code: String, up: Boolean) -> Boolean,
    private val consumed: () -> Unit,
) {
    fun down(code: String): Boolean {
        if (code !in KeyMap.MODIFIERS && latches.isNotEmpty()) {
            val mods = latches.toList()
            active[code] = mods.toSet()
            if (!mods.all { send(it, false) }) return false
            latches.clear()
            consumed()
        }
        return send(code, false)
    }

    fun up(code: String): Boolean {
        var ok = send(code, true)
        active.remove(code)?.reversed()?.forEach { ok = send(it, true) && ok }
        return ok
    }
}
