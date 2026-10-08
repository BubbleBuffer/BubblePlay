package de.bubblebuffer.bubbleplay.net

import de.bubblebuffer.bubbleplay.protocol.CursorKind
import de.bubblebuffer.bubbleplay.protocol.CursorState
import de.bubblebuffer.bubbleplay.protocol.CursorType
import okio.ByteString

/** One queued delivery, with the latest position and appearance from the same socket. */
class PendingCursor<S> {
    data class Update<S>(val source: S, val state: CursorState?, val kind: CursorKind?)

    private var pending: Update<S>? = null

    /** False means rejected or already queued. Shape is checked before copying any bytes. */
    @Synchronized
    fun offer(source: S, bytes: ByteString): Boolean {
        if (!accepts(bytes)) return false
        val type = bytes[0].toInt() and 0xff
        val packet = bytes.toByteArray()
        val previous = pending?.takeIf { it.source === source }
        val enqueue = pending == null
        pending = Update(source,
            if (type == 128) CursorState.decode(packet) else previous?.state,
            if (type == CursorType.PACKET_TYPE) CursorType.decode(packet) else previous?.kind)
        return enqueue
    }

    @Synchronized
    fun take(): Update<S>? {
        val update = pending
        pending = null
        return update
    }

    companion object {
        fun accepts(bytes: ByteString): Boolean {
            if (bytes.size == 0) return false
            return when (bytes[0].toInt() and 0xff) {
                128 -> bytes.size == 8
                CursorType.PACKET_TYPE -> bytes.size == CursorType.PACKET_BYTES
                else -> false
            }
        }
    }
}
