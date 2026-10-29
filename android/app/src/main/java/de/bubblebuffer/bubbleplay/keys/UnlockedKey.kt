package de.bubblebuffer.bubbleplay.keys

/**
 * The device key unlocked for this app process. The app unlocks it once on open (mandatory) and
 * keeps it until the process ends or the key is replaced or removed; sessions and pairing read it
 * from here. Nothing is persisted.
 */
object UnlockedKey {
    @Volatile
    private var key: DeviceKey? = null

    fun get(): DeviceKey? = key?.takeUnless { it.isClosed }

    /** The unlocked key, or IllegalStateException when the app has not unlocked one. */
    fun require(): DeviceKey = get() ?: throw IllegalStateException("device key is locked")

    /** Takes ownership of [unlocked]; a previously held key is closed. */
    @Synchronized
    fun set(unlocked: DeviceKey) {
        val old = key
        key = unlocked
        if (old != null && old !== unlocked) old.close()
    }

    @Synchronized
    fun clear() {
        key?.close()
        key = null
    }
}
