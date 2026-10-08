package de.bubblebuffer.bubbleplay.ui

/** Each injected delete repeat must release the key before the next press. */
internal object RemoteKeyRepeats {
    fun tap(count: Int, send: (up: Boolean) -> Boolean): Boolean {
        repeat(count) {
            if (!send(false) || !send(true)) return false
        }
        return true
    }
}
