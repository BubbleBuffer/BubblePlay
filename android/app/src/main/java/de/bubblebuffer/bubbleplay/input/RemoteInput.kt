package de.bubblebuffer.bubbleplay.input

import de.bubblebuffer.bubbleplay.protocol.Clipboard
import de.bubblebuffer.bubbleplay.protocol.InputPackets
import de.bubblebuffer.bubbleplay.protocol.KeyMap
import java.util.ArrayDeque
import kotlin.math.roundToInt

/**
 * Ordered input to the host (the essentials of input.js InputController).
 * Control packets (keys, buttons, text, clipboard) are sent in order; the
 * WebSocket buffers them. Pointer motion is coalesced and retained under
 * congestion. Deferred motion and reliable controls retain their event order.
 * While the transport is down nothing is sent, and the next live packet is
 * preceded by release-all so the host never keeps keys from an earlier session.
 * A monitor-transition gate (the transport is alive and [Transport.inputGated]
 * is true) keeps held keys and buttons: refused packets are dropped, pending
 * motion is discarded, and release-all follows only if held input was let go
 * during the gate ([resumeAfterTransition]). Like input.js InputController.
 */
class RemoteInput(
    private val transport: Transport,
    /** Cursor authority barriers only; wheel-only sends still advance [lastSentSequence]. */
    private val onSequenceSent: (Int) -> Unit = {},
    private val onDeferredInput: () -> Unit = {},
) {
    interface Transport {
        /** True while the signaling socket is open and the host accepts input. */
        fun inputOpen(): Boolean
        /** True while a monitor transition holds input on a live transport (only consulted when [inputOpen] is false). */
        fun inputGated(): Boolean = false
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
    private var pendingDx = 0L
    private var pendingDy = 0L
    private var pendingWheelX = 0L
    private var pendingWheelY = 0L
    private var pendingAbsolute: Pair<Int, Int>? = null
    private sealed interface Pending
    private data class Reliable(val packet: ByteArray, val sequenced: Boolean = false) : Pending
    private data class Motion(
        var dx: Long, var dy: Long, var wx: Long, var wy: Long,
        var absolute: Pair<Int, Int>?,
    ) : Pending {
        fun empty() = absolute == null && dx == 0L && dy == 0L && wx == 0L && wy == 0L
    }
    private val deferred = ArrayDeque<Pending>()

    /** True means queued locally for this live session, not acknowledged by the host. */
    @Synchronized
    fun control(packet: ByteArray?): Boolean {
        if (packet == null) return false
        return enqueueControl(packet)
    }

    /** True means accepted into this session's ordered queue, including a deferred send. */
    private fun enqueueControl(packet: ByteArray, sequenced: Boolean = false): Boolean {
        if (!transport.inputOpen()) return refuse(releasesHeld = false)
        freezeMotion()
        deferred.addLast(Reliable(packet, sequenced))
        drain()
        if (hasPendingInput()) onDeferredInput()
        return true
    }

    /**
     * Refuses a packet while input is closed. Dead transport or a desktop-source change: discard
     * everything and send release-all with the next live packet. Monitor gate: drop pending motion
     * only; held input is kept, and release-all is marked only when the caller let held input go.
     */
    private fun refuse(releasesHeld: Boolean): Boolean {
        if (gated()) {
            dropGatedMotion()
            if (releasesHeld) needsRelease = true
        } else {
            discardPending(); needsRelease = true
        }
        return false
    }

    private fun gated() = !transport.inputOpen() && transport.inputGated()

    private fun dropGatedMotion() { clearMotion(); deferred.removeAll { it is Motion } }

    /** Physical key by DOM code; scan code on capable hosts, VK otherwise. */
    @Synchronized
    fun key(code: String, up: Boolean): Boolean {
        val packet = KeyMap.packet(code, up, transport.scancodeHost()) ?: return false
        if (gated()) {
            // A gated press is not recorded; a release of a key the host still holds needs release-all on resume.
            val wasHeld = code in held
            if (up) held.remove(code)
            return refuse(releasesHeld = up && wasHeld)
        }
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
    fun buttons(mask: Int): Boolean {
        if (gated()) return refuse(releasesHeld = (buttonMask and mask.inv()) != 0)
        if (!control(InputPackets.buttons(mask))) return false
        buttonMask = mask
        return true
    }

    /** Buttons at an absolute position (normalized 0..65535), with a sequence barrier. */
    @Synchronized
    fun buttonsAt(x: Int, y: Int, mask: Int): Boolean {
        if (gated()) return refuse(releasesHeld = (buttonMask and mask.inv()) != 0)
        if (!enqueueControl(InputPackets.buttonsAt(0, x.coerceIn(0, 65535), y.coerceIn(0, 65535), mask), sequenced = true)) return false
        buttonMask = mask
        return true
    }

    @Synchronized
    fun cursorSync(): Boolean {
        clearMotion()
        return enqueueControl(InputPackets.cursorSync(0), sequenced = true)
    }

    @Synchronized
    fun motion(dx: Int, dy: Int, wheelX: Int = 0, wheelY: Int = 0) {
        // A relative sample after an unsent absolute target follows that target.
        // drain() sends the target first, then the accumulated relative/wheel tail.
        pendingDx += dx; pendingDy += dy; pendingWheelX += wheelX; pendingWheelY += wheelY
    }

    @Synchronized
    fun absolute(x: Int, y: Int) {
        // A newer target supersedes earlier position deltas in this unfenced
        // segment; wheel distance is independent and must remain pending.
        pendingDx = 0L; pendingDy = 0L
        pendingAbsolute = x.coerceIn(0, 65535) to y.coerceIn(0, 65535)
    }

    fun absoluteNormalized(nx: Float, ny: Float) = absolute((nx * 65535f).roundToInt(), (ny * 65535f).roundToInt())

    /** Sends coalesced motion; keeps it pending while the socket is congested. */
    @Synchronized
    fun flushMotion(): Boolean {
        return drain()
    }

    private fun drain(): Boolean {
        if (!transport.inputOpen()) { refuse(releasesHeld = false); return false }
        var sent = false
        while (true) {
            // Keep unfenced motion live so new events can still coalesce into it.
            if (deferred.isEmpty()) freezeMotion()
            val next = deferred.peekFirst() ?: return sent
            val packet = when (next) {
                is Reliable -> if (next.sequenced) next.packet.copyOf().also {
                    it[1] = seq.toByte(); it[2] = (seq ushr 8).toByte()
                } else next.packet
                is Motion -> next.absolute?.let { InputPackets.absolute(seq, it.first, it.second) }
                    ?: InputPackets.motion(seq, chunk(next.dx), chunk(next.dy), chunk(next.wx), chunk(next.wy))
            }
            val releaseBytes = if (needsRelease) 1 else 0
            if (next is Motion && transport.queuedBytes() + packet.size + releaseBytes > MOTION_BACKLOG_BYTES) {
                thawUnfencedMotion()
                return sent
            }
            if (needsRelease) {
                if (!transport.send(InputPackets.releaseAll())) { thawUnfencedMotion(); return sent }
                needsRelease = false
            }
            if (!transport.send(packet)) { thawUnfencedMotion(); return sent }
            sent = true
            when (next) {
                is Reliable -> {
                    deferred.removeFirst()
                    if (next.sequenced) noteSequenceSent()
                }
                is Motion -> {
                    val movesCursor = next.absolute != null || next.dx != 0L || next.dy != 0L
                    if (next.absolute != null) next.absolute = null
                    else {
                        next.dx -= chunk(next.dx); next.dy -= chunk(next.dy)
                        next.wx -= chunk(next.wx); next.wy -= chunk(next.wy)
                    }
                    noteSequenceSent(cursorAuthority = movesCursor)
                    if (next.empty()) deferred.removeFirst()
                }
            }
        }
    }

    private fun noteSequenceSent(cursorAuthority: Boolean = true) {
        lastSentSequence = seq
        if (cursorAuthority) onSequenceSent(seq)
        seq = (seq + 1) and 0xffff
    }

    private fun freezeMotion() {
        if (!hasLiveMotion()) return
        deferred.addLast(Motion(pendingDx, pendingDy, pendingWheelX, pendingWheelY, pendingAbsolute))
        clearMotion()
    }

    /** A refused final segment has no control barrier; continue coalescing into it. */
    private fun thawUnfencedMotion() {
        if (deferred.size != 1 || deferred.peekFirst() !is Motion) return
        val motion = deferred.removeFirst() as Motion
        pendingDx += motion.dx; pendingDy += motion.dy
        pendingWheelX += motion.wx; pendingWheelY += motion.wy
        if (pendingAbsolute == null) pendingAbsolute = motion.absolute
    }

    private fun hasLiveMotion() = pendingAbsolute != null || pendingDx != 0L || pendingDy != 0L || pendingWheelX != 0L || pendingWheelY != 0L
    private fun chunk(value: Long) = value.coerceIn(-32768L, 32767L).toInt()

    fun hasPendingMotion(): Boolean = synchronized(this) {
        hasLiveMotion() || deferred.any { it is Motion }
    }

    /** Unsent cursor coordinates/displacement; wheel alone needs no prediction protection. */
    fun hasPendingCursorMotion(): Boolean = synchronized(this) {
        pendingAbsolute != null || pendingDx != 0L || pendingDy != 0L || deferred.any {
            it is Motion && (it.absolute != null || it.dx != 0L || it.dy != 0L)
        }
    }

    fun hasPendingInput(): Boolean = synchronized(this) { hasLiveMotion() || deferred.isNotEmpty() }

    @Synchronized
    fun releaseAll(send: Boolean = true) {
        held.clear(); buttonMask = 0; discardPending(); lastSentSequence = null
        if (send && transport.inputOpen()) control(InputPackets.releaseAll()) else needsRelease = true
    }

    /**
     * The monitor-transition gate opened (called before the cursor sync). Sends release-all
     * only if held input was let go during the gate, so a carried drag continues; then drains
     * whatever was queued before the gate.
     */
    @Synchronized
    fun resumeAfterTransition() {
        if (!transport.inputOpen()) return
        if (needsRelease) {
            held.clear(); buttonMask = 0; needsRelease = false
            enqueueControl(InputPackets.releaseAll())
        } else {
            drain()
        }
    }

    private fun clearMotion() {
        pendingDx = 0L; pendingDy = 0L; pendingWheelX = 0L; pendingWheelY = 0L; pendingAbsolute = null
    }

    private fun discardPending() { clearMotion(); deferred.clear() }

    companion object {
        const val MOTION_BACKLOG_BYTES = 512L
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
        // RemoteInput splits whole distance into i16 packets without loss.
        return ix to iy
    }
}
