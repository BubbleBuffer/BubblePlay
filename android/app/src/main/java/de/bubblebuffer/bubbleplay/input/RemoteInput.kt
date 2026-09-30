package de.bubblebuffer.bubbleplay.input

import de.bubblebuffer.bubbleplay.protocol.Clipboard
import de.bubblebuffer.bubbleplay.protocol.InputPackets
import de.bubblebuffer.bubbleplay.protocol.KeyMap
import kotlin.math.roundToInt

/**
 * Ordered input to the host (the essentials of input.js InputController).
 * Control packets (keys, buttons, text, clipboard) are sent in order; the
 * WebSocket buffers them. Pointer motion is coalesced and dropped under
 * congestion. While the transport is down or input is gated nothing is sent,
 * and the next live packet is preceded by release-all so the host never keeps
 * keys from an earlier session.
 */
class RemoteInput(private val transport: Transport) {
    interface Transport {
        /** True while the signaling socket is open and the host accepts input. */
        fun inputOpen(): Boolean
        fun send(bytes: ByteArray): Boolean
        fun queuedBytes(): Long
        /** Host understands type-13 scan-code packets. */
        fun scancodeHost(): Boolean
    }

    private var seq = 0
    private var needsRelease = false
    private val held = LinkedHashSet<String>()
    var buttonMask = 0; private set
    var lastSentSequence: Int? = null; private set
    private var clipboardId = 0

    // Coalesced motion.
    private var pendingDx = 0
    private var pendingDy = 0
    private var pendingWheelX = 0
    private var pendingWheelY = 0
    private var pendingAbsolute: Pair<Int, Int>? = null

    @Synchronized
    fun control(packet: ByteArray?): Boolean {
        if (packet == null) return false
        if (!transport.inputOpen()) { needsRelease = true; return false }
        if (needsRelease) {
            if (!transport.send(InputPackets.releaseAll())) return false
            needsRelease = false
        }
        return transport.send(packet)
    }

    /** Physical key by DOM code; scan code on capable hosts, VK otherwise. */
    @Synchronized
    fun key(code: String, up: Boolean): Boolean {
        val packet = KeyMap.packet(code, up, transport.scancodeHost()) ?: return false
        if (up) held.remove(code) else held.add(code)
        return control(packet)
    }

    fun tap(code: String) { key(code, false); key(code, true) }

    /** Unicode text, split into packets of at most 512 UTF-8 bytes on code-point boundaries. */
    @Synchronized
    fun text(value: String): Boolean {
        if (value.isEmpty()) return false
        var ok = true
        val chunk = StringBuilder()
        var bytes = 0
        var i = 0
        while (i < value.length) {
            val cp = value.codePointAt(i)
            val s = String(Character.toChars(cp))
            val n = s.toByteArray(Charsets.UTF_8).size
            if (bytes + n > InputPackets.TEXT_MAX_BYTES) {
                ok = control(InputPackets.text(chunk.toString())) && ok
                chunk.setLength(0); bytes = 0
            }
            chunk.append(s); bytes += n
            i += Character.charCount(cp)
        }
        if (chunk.isNotEmpty()) ok = control(InputPackets.text(chunk.toString())) && ok
        return ok
    }

    /** Sets the host clipboard (and pastes when [paste]). */
    @Synchronized
    fun clipboard(text: String, paste: Boolean = true): Boolean {
        clipboardId = (clipboardId + 1) and 0xffff
        val packets = Clipboard.packets(text, clipboardId, paste) ?: return false
        return packets.all { control(it) }
    }

    @Synchronized
    fun buttons(mask: Int): Boolean { buttonMask = mask; return control(InputPackets.buttons(mask)) }

    /** Buttons at an absolute position (normalized 0..65535), with a sequence barrier. */
    @Synchronized
    fun buttonsAt(x: Int, y: Int, mask: Int): Boolean {
        clearMotion()
        val s = seq
        if (!control(InputPackets.buttonsAt(s, x.coerceIn(0, 65535), y.coerceIn(0, 65535), mask))) return false
        lastSentSequence = s; seq = (s + 1) and 0xffff; buttonMask = mask
        return true
    }

    @Synchronized
    fun cursorSync(): Boolean {
        clearMotion()
        val s = seq
        if (!control(InputPackets.cursorSync(s))) return false
        lastSentSequence = s; seq = (s + 1) and 0xffff
        return true
    }

    @Synchronized
    fun motion(dx: Int, dy: Int, wheelX: Int = 0, wheelY: Int = 0) {
        pendingAbsolute = null
        pendingDx += dx; pendingDy += dy; pendingWheelX += wheelX; pendingWheelY += wheelY
    }

    @Synchronized
    fun absolute(x: Int, y: Int) {
        pendingDx = 0; pendingDy = 0
        pendingAbsolute = x.coerceIn(0, 65535) to y.coerceIn(0, 65535)
    }

    fun absoluteNormalized(nx: Float, ny: Float) = absolute((nx * 65535f).roundToInt(), (ny * 65535f).roundToInt())

    /** Sends coalesced motion; keeps it pending while the socket is congested. */
    @Synchronized
    fun flushMotion(): Boolean {
        val abs = pendingAbsolute
        val hasRelative = pendingDx != 0 || pendingDy != 0 || pendingWheelX != 0 || pendingWheelY != 0
        if (abs == null && !hasRelative) return false
        if (!transport.inputOpen()) { clearMotion(); return false }
        if (transport.queuedBytes() > MOTION_BACKLOG_BYTES) return false
        val s = seq
        val packet = if (abs != null) InputPackets.absolute(s, abs.first, abs.second)
        else InputPackets.motion(s, pendingDx, pendingDy, pendingWheelX, pendingWheelY)
        if (!control(packet)) return false
        clearMotion()
        lastSentSequence = s; seq = (s + 1) and 0xffff
        return true
    }

    fun hasPendingMotion(): Boolean = synchronized(this) {
        pendingAbsolute != null || pendingDx != 0 || pendingDy != 0 || pendingWheelX != 0 || pendingWheelY != 0
    }

    @Synchronized
    fun releaseAll(send: Boolean = true) {
        held.clear(); buttonMask = 0; clearMotion()
        if (send) control(InputPackets.releaseAll()) else needsRelease = true
    }

    private fun clearMotion() {
        pendingDx = 0; pendingDy = 0; pendingWheelX = 0; pendingWheelY = 0; pendingAbsolute = null
    }

    companion object {
        const val MOTION_BACKLOG_BYTES = 16_384L
    }
}

/**
 * Accumulates fractional wheel distance in WHEEL_DELTA units (120 per notch)
 * and emits whole units, carrying the remainder (input.js WheelAccumulator).
 */
class WheelAccumulator {
    private var x = 0f
    private var y = 0f

    fun reset() { x = 0f; y = 0f }

    /** Adds units; returns the whole units to send now. */
    fun add(unitsX: Float, unitsY: Float): Pair<Int, Int> {
        x += unitsX; y += unitsY
        val ix = x.toInt(); val iy = y.toInt()
        x -= ix; y -= iy
        return ix.coerceIn(-32768, 32767) to iy.coerceIn(-32768, 32767)
    }
}
