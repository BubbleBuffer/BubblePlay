package de.bubblebuffer.bubbleplay.log

import org.json.JSONArray
import org.json.JSONObject
import java.util.ArrayDeque
import java.util.concurrent.ScheduledThreadPoolExecutor
import java.util.concurrent.TimeUnit
import java.util.concurrent.atomic.AtomicBoolean

/**
 * The connection the uploader talks over: the authenticated, sealed ctl channel of one session. The
 * uploader knows nothing else about the session, so tests use a fake.
 */
interface LogUploadTransport {
    /** Receives every incoming (already decrypted) JSON message; null removes the listener. Any thread. */
    fun setListener(listener: ((JSONObject) -> Unit)?)

    /** Sends one message; false when it could not be queued (the uploader tries again later). Never blocks. */
    fun send(message: JSONObject): Boolean

    /** Bytes waiting in the ctl send buffer. */
    val bufferedBytes: Long

    /** Milliseconds since the last input event was sent; a very large value when there was none. */
    val sinceLastInputMs: Long
}

/** Runs the uploader's work on one thread; tests substitute a manual one. */
internal interface UploadScheduler {
    fun post(delayMs: Long, task: () -> Unit)

    fun shutdown()
}

private class ThreadScheduler : UploadScheduler {
    private val executor = ScheduledThreadPoolExecutor(1) { r -> Thread(r, "bp-log-upload").apply { isDaemon = true } }
        .apply { removeOnCancelPolicy = true }

    override fun post(delayMs: Long, task: () -> Unit) {
        try {
            executor.schedule({
                try {
                    task()
                } catch (_: Throwable) {
                }
            }, delayMs.coerceAtLeast(0), TimeUnit.MILLISECONDS)
        } catch (_: Throwable) {
        }
    }

    override fun shutdown() {
        try {
            executor.shutdownNow()
        } catch (_: Throwable) {
        }
    }
}

/**
 * Client side of docs/architecture/client-log-upload.md for the stream `app-android-core`.
 *
 * Usage: create one per authenticated connection, call [start], call [close] when the connection ends.
 * After `logs-state` it sends the stored lines of [pcId] (and lines without a PC) after the host's cursor,
 * oldest first (history), then lines as they are written (live). One batch is in flight; the next is sent
 * after its ack. A message is sent only when the ctl send buffer is at most 16 KiB, no input was sent in the
 * last 250 ms and `minBatchIntervalMs` has passed since the last batch. Lines are sent exactly as stored.
 *
 * Everything runs on its own single worker thread and nothing throws into the caller. The uploader writes no
 * log lines itself (that would feed back into the queue).
 */
class LogUploader internal constructor(
    private val transport: LogUploadTransport,
    private val bpLog: BpLog,
    private val pcId: String,
    private val clock: () -> Long,
    private val scheduler: UploadScheduler,
) {
    constructor(
        transport: LogUploadTransport,
        bpLog: BpLog,
        pcId: String,
        clock: () -> Long = System::currentTimeMillis,
    ) : this(transport, bpLog, pcId, clock, ThreadScheduler())

    private class Batch(val id: Long, val from: String, val to: String, val live: Boolean, val count: Int, val lineBytes: Long)

    // Everything below is touched only on the scheduler thread.
    private val buffer = ArrayDeque<CursorLine>()
    private var confirmed = ""
    private var ready = false
    private var stopped = false
    private var inFlight: Batch? = null
    private var nextId = 1L
    private var lastSendAt = Long.MIN_VALUE / 2
    private var retryAt = 0L
    private var sentBytes = 0L
    private var liveFromTs = ""
    private var timerAt = -1L
    private var maxBatchBytes = MAX_BATCH_BYTES
    private var maxBatchLines = MAX_BATCH_LINES
    private var minIntervalMs = MIN_INTERVAL_MS
    private var budgetBytes = Long.MAX_VALUE

    @Volatile private var started = false
    @Volatile private var closed = false
    private val wakePending = AtomicBoolean(false)
    private val wake: () -> Unit = {
        if (!closed && wakePending.compareAndSet(false, true)) {
            scheduler.post(0) {
                wakePending.set(false)
                pump()
            }
        }
    }

    /** Begins listening. Call once on an authenticated connection; it waits for `logs-state`. */
    fun start() {
        try {
            if (started || closed) return
            started = true
            transport.setListener { message -> onMessage(message) }
            bpLog.addWriteListener(wake)
        } catch (_: Throwable) {
        }
    }

    /** Stops for good: no more sends, the worker thread ends. */
    fun close() {
        try {
            closed = true
            try {
                transport.setListener(null)
            } catch (_: Throwable) {
            }
            bpLog.removeWriteListener(wake)
            scheduler.shutdown()
        } catch (_: Throwable) {
        }
    }

    private fun onMessage(message: JSONObject) {
        if (closed) return
        scheduler.post(0) { handle(message) }
    }

    private fun schedule(delayMs: Long) {
        if (closed) return
        val at = clock() + delayMs
        if (timerAt != -1L && timerAt <= at) return
        timerAt = at
        scheduler.post(delayMs) {
            if (timerAt == at) timerAt = -1L
            pump()
        }
    }

    private fun handle(message: JSONObject) {
        try {
            if (closed || message.optInt("v", 0) != 1) return
            when (message.optString("type")) {
                "logs-state" -> onState(message)
                "logs-ack" -> onAck(message)
            }
        } catch (_: Throwable) {
        }
    }

    private fun onState(message: JSONObject) {
        if (ready) return
        val limits = message.optJSONObject("limits")
        if (limits != null) {
            maxBatchBytes = limits.optInt("maxBatchBytes", MAX_BATCH_BYTES).coerceIn(1, MAX_BATCH_BYTES)
            maxBatchLines = limits.optInt("maxBatchLines", MAX_BATCH_LINES).coerceIn(1, MAX_BATCH_LINES)
            minIntervalMs = limits.optLong("minBatchIntervalMs", MIN_INTERVAL_MS).coerceAtLeast(0)
            budgetBytes = limits.optLong("connectionBudgetBytes", Long.MAX_VALUE).let { if (it <= 0) Long.MAX_VALUE else it }
        }
        confirmed = message.optJSONObject("cursors")?.optString(BpLog.SRC, "") ?: ""
        liveFromTs = BpLog.formatTs(clock())
        ready = true
        pump()
    }

    private fun onAck(message: JSONObject) {
        val batch = inFlight ?: return
        if (message.optString("stream") != BpLog.SRC || message.optLong("id", -1) != batch.id) return
        when (message.optString("status")) {
            "ok" -> {
                val cursor = message.optString("cursor", "")
                advance(if (cursor.isNotEmpty()) cursor else batch.to)
                pump()
            }
            "rate-limited" -> {
                val wait = message.optLong("retryAfterMs", 0).let { if (it > 0) it else minIntervalMs }
                retryAt = clock() + wait
                pump()
            }
            "budget" -> {
                stopped = true
                inFlight = null
            }
            "rejected" -> {
                advance(batch.to) // the batch is not retried
                pump()
            }
        }
    }

    private fun advance(cursor: String) {
        inFlight = null
        retryAt = 0
        confirmed = cursor
        while (buffer.isNotEmpty() && buffer.peekFirst()!!.cursor <= cursor) buffer.pollFirst()
    }

    private fun pump() {
        try {
            if (closed || stopped || !ready) return
            val now = clock()
            val batch = inFlight
            if (batch != null && retryAt == 0L) return // waiting for the ack
            if (batch == null) {
                if (buffer.isEmpty()) {
                    refill()
                    if (buffer.isEmpty()) return // idle until the log writes more
                }
            }
            // Priority gate: never hurt the stream.
            val earliest = maxOf(lastSendAt + minIntervalMs, retryAt)
            if (now < earliest) return schedule(earliest - now)
            if (transport.bufferedBytes > MAX_BUFFERED) return schedule(RECHECK_MS)
            val sinceInput = transport.sinceLastInputMs
            if (sinceInput < QUIET_MS) return schedule(QUIET_MS - sinceInput)

            if (batch == null && buffer.size < maxBatchLines) refill() // coalesce lines written while waiting
            val toSend = batch ?: nextBatch(now)
            if (sentBytes + toSend.lineBytes > budgetBytes) {
                stopped = true
                inFlight = null
                return
            }
            if (transport.send(message(toSend))) {
                lastSendAt = now
                retryAt = 0
                sentBytes += toSend.lineBytes
            } else {
                retryAt = now + RECHECK_MS * 10
                schedule(RECHECK_MS * 10)
            }
        } catch (_: Throwable) {
        }
    }

    private fun refill() {
        buffer.clear()
        buffer.addAll(bpLog.readAfter(confirmed, pcId, READ_LINES, READ_BYTES))
    }

    private fun nextBatch(now: Long): Batch {
        var count = 0
        var bytes = 0L
        var last: CursorLine? = null
        for (line in buffer) {
            if (count >= maxBatchLines) break
            val cost = lineCost(line.line)
            if (count > 0 && bytes + cost > maxBatchBytes) break
            bytes += cost
            count++
            last = line
        }
        val first = buffer.peekFirst()!!
        val batch = Batch(nextId++, confirmed, last!!.cursor, first.ts >= liveFromTs, count, bytes)
        inFlight = batch
        return batch
    }

    /** What the host counts for a line: its UTF-8 bytes plus the newline. */
    private fun lineCost(line: String): Int = line.toByteArray(Charsets.UTF_8).size + 1

    private fun message(batch: Batch): JSONObject {
        val lines = JSONArray()
        var n = 0
        for (line in buffer) {
            if (n++ >= batch.count) break
            lines.put(line.line)
        }
        return JSONObject()
            .put("type", "logs-batch")
            .put("v", 1)
            .put("stream", BpLog.SRC)
            .put("id", batch.id)
            .put("from", batch.from)
            .put("to", batch.to)
            .put("live", batch.live)
            .put("lines", lines)
    }

    private companion object {
        const val MAX_BATCH_BYTES = 16 * 1024
        const val MAX_BATCH_LINES = 100
        const val MIN_INTERVAL_MS = 500L
        const val MAX_BUFFERED = 16L * 1024
        const val QUIET_MS = 250L
        const val RECHECK_MS = 100L
        const val READ_LINES = 400
        const val READ_BYTES = 96 * 1024
    }
}
