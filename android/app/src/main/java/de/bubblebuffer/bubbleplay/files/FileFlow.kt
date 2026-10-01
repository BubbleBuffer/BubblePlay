package de.bubblebuffer.bubbleplay.files

import de.bubblebuffer.bubbleplay.protocol.FileWire
import java.security.MessageDigest

/**
 * Sender-side flow control: at most [limit] bytes un-ACKed. ACKs carry the
 * receiver's running total, so they must never go backwards or past what was
 * sent.
 */
class SendWindow(private val limit: Long = FileWire.WINDOW) {
    var sent = 0L
        private set
    var acked = 0L
        private set

    val inFlight: Long get() = sent - acked

    /** How many bytes may go out now (0 when the window is full). */
    fun room(): Long = (limit - inFlight).coerceAtLeast(0)

    fun canSend(bytes: Int): Boolean = bytes <= room()

    fun onSent(bytes: Int) {
        require(canSend(bytes)) { "window exceeded" }
        sent += bytes
    }

    /** False for an ACK that goes backwards or claims more than was sent (a protocol error). */
    fun onAck(total: Long): Boolean {
        if (total < acked || total > sent) return false
        acked = total
        return true
    }
}

/**
 * Receiver-side ACK pacing: ACK once 256 KiB arrived since the last ACK, or
 * when data has been waiting [FileWire.ACK_MS]; [dueAt] is when the timer
 * should fire.
 */
class AckSchedule(private val everyBytes: Long = FileWire.ACK_BYTES, private val everyMs: Long = FileWire.ACK_MS) {
    var received = 0L
        private set
    var lastAcked = 0L
        private set
    private var pendingSince = -1L

    /** Bytes arrived; true when an ACK must go out now. */
    fun onData(bytes: Int, now: Long): Boolean {
        received += bytes
        if (pendingSince < 0) pendingSince = now
        return received - lastAcked >= everyBytes || now - pendingSince >= everyMs
    }

    /** Timer check. */
    fun due(now: Long): Boolean = received > lastAcked && pendingSince >= 0 && now - pendingSince >= everyMs

    /** When the timer should next look, or -1 if nothing is waiting. */
    fun dueAt(): Long = if (received > lastAcked && pendingSince >= 0) pendingSince + everyMs else -1

    /** An ACK for [received] went out. */
    fun acked(): Long {
        lastAcked = received
        pendingSince = -1
        return received
    }
}

/**
 * What a receiver checks before RESULT ok: chunks arrive in order, never past
 * the announced size, and DONE's SHA-256 matches what was written.
 */
class ReceiveCheck(val size: Long) {
    private val digest = MessageDigest.getInstance("SHA-256")
    var received = 0L
        private set
    private var nextSeq = 0L

    /** Null when the chunk is acceptable (and is hashed), else the error code. */
    fun onData(seq: Long, chunk: ByteArray): String? {
        if (seq != nextSeq) return ERR_SEQUENCE
        if (received + chunk.size > size) return ERR_SIZE
        digest.update(chunk)
        received += chunk.size
        nextSeq++
        return null
    }

    /** Null when the file is complete and intact, else the error code for RESULT. */
    fun onDone(sha256: ByteArray): String? {
        if (received != size) return ERR_SIZE
        if (!MessageDigest.isEqual(digest.digest(), sha256)) return ERR_HASH
        return null
    }

    companion object {
        const val ERR_SEQUENCE = "seq-gap"
        const val ERR_SIZE = "size-mismatch"
        const val ERR_HASH = "hash-mismatch"
    }
}
