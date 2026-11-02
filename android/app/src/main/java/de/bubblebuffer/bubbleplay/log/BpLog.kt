package de.bubblebuffer.bubbleplay.log

import android.content.Context
import java.io.File
import java.io.FileOutputStream
import java.io.PrintWriter
import java.io.StringWriter
import java.time.Instant
import java.time.ZoneOffset
import java.time.format.DateTimeFormatter
import java.util.concurrent.ArrayBlockingQueue
import java.util.concurrent.CopyOnWriteArrayList
import java.util.concurrent.TimeUnit
import java.util.concurrent.atomic.AtomicInteger
import java.util.concurrent.atomic.AtomicLong

enum class LogLevel(val wire: String, internal val priority: Int) {
    TRACE("trace", 2), DEBUG("debug", 3), INFO("info", 4), WARN("warn", 5), ERROR("error", 6),
}

/** Size and age limits of the client store (logging spec section 3). */
data class LogLimits(
    val perPcBytes: Long = 5L * 1024 * 1024,
    val totalBytes: Long = 20L * 1024 * 1024,
    val retentionDays: Int = 30,
)

/**
 * The Kotlin core's JSONL log (logging spec): one JSON object per line, written under
 * `<root>/pc/<pc-id>/app-android-core-YYYY-MM-DD.jsonl` (lines tied to a PC) or
 * `<root>/misc/app-android-core-YYYY-MM-DD.jsonl`. In the app the root is `filesDir/logs`.
 *
 * Callers never block and never see an exception: lines are redacted and serialized on the calling
 * thread, then handed to one writer thread through a bounded queue that drops the oldest line when
 * full (the count is logged once per minute). The same redacted text is mirrored to logcat.
 */
class BpLog internal constructor(
    private val root: File,
    private val clock: () -> Long = System::currentTimeMillis,
    private val mirror: (LogLevel, String) -> Unit = { level, text -> logcat(level, text) },
    queueCapacity: Int = 1024,
    private val limits: LogLimits = LogLimits(),
    private val minLevel: LogLevel = LogLevel.INFO,
    startWriter: Boolean = true,
) {
    private class Entry(val pc: String?, val day: String, val line: String)

    private class Built(val line: String, val mirrorText: String)

    private val queue = ArrayBlockingQueue<Entry>(queueCapacity)
    private val pending = AtomicInteger()
    private val dropped = AtomicLong()
    private val ioLock = Any()
    private val secrets = CopyOnWriteArrayList<String>()
    private var bytesSinceSweep = Long.MAX_VALUE // forces a sweep after the first write
    @Volatile private var device: String? = null
    @Volatile private var lastDropReport = clock()
    @Volatile private var lastDay = ""
    @Volatile private var closed = false
    private var writer: Thread? = null

    init {
        if (startWriter) {
            writer = Thread({ writerLoop() }, "bp-log").apply { isDaemon = true; start() }
        }
    }

    /** Own device id for the `device` field; ignored when it is not a valid id. */
    fun setDevice(id: String?) {
        device = id?.takeIf { SAFE_ID.matches(it) }
    }

    /** Registers a literal that must never reach a file (rule 5). Call before it is used. */
    fun registerSecret(secret: String) {
        if (secret.isNotEmpty() && secret !in secrets) secrets.add(secret)
    }

    fun log(
        level: LogLevel,
        target: String,
        msg: String,
        pc: String? = null,
        session: String? = null,
        fields: Map<String, Any?> = emptyMap(),
        error: Throwable? = null,
    ) = write(level, target, msg, pc, session, fields, error, sync = false)

    private fun write(
        level: LogLevel,
        target: String,
        msg: String,
        pc: String?,
        session: String?,
        fields: Map<String, Any?>,
        error: Throwable?,
        sync: Boolean,
    ) {
        try {
            if (closed || level.priority < minLevel.priority) return
            val now = clock()
            val pcId = pc?.takeIf { SAFE_ID.matches(it) }
            val built = build(level, target, msg, pcId, session, fields, error, now)
            try {
                mirror(level, built.mirrorText)
            } catch (_: Throwable) {
            }
            val entry = Entry(pcId, dayOf(now), built.line)
            if (sync) {
                synchronized(ioLock) {
                    drainLocked()
                    pending.incrementAndGet()
                    writeBatchLocked(listOf(entry))
                }
            } else {
                enqueue(entry)
            }
        } catch (_: Throwable) {
            // Logging never throws into callers.
        }
    }

    private fun enqueue(entry: Entry) {
        pending.incrementAndGet()
        while (!queue.offer(entry)) {
            if (queue.poll() != null) {
                pending.decrementAndGet()
                dropped.incrementAndGet()
            }
        }
    }

    private fun build(
        level: LogLevel,
        target: String,
        msg: String,
        pc: String?,
        session: String?,
        fields: Map<String, Any?>,
        error: Throwable?,
        now: Long,
    ): Built {
        val ts = TS.format(Instant.ofEpochMilli(now))
        val snapshot = secrets.toList()
        var finalMsg: String
        var fieldsJson: String
        var finalTarget = target
        try {
            finalTarget = Redact.text(target, snapshot)
            finalMsg = Redact.text(msg, snapshot)
            val all = LinkedHashMap<String, Any?>(fields)
            if (error != null) {
                all["error"] = error.javaClass.name + (error.message?.let { ": $it" } ?: "")
                all["stack"] = stackOf(error)
            }
            val redacted = LinkedHashMap<String, Any?>()
            for ((k, v) in all) {
                val value = Redact.field(k, v, snapshot)
                redacted[Redact.text(k, snapshot)] = if (k == "stack" && value is String) value.take(MAX_STACK) else value
            }
            fieldsJson = Json.write(redacted)
        } catch (_: Throwable) {
            finalMsg = "[redaction failed]"
            fieldsJson = "{}"
        }
        fun assemble(m: String, f: String) = buildString {
            append("{\"ts\":").append(Json.str(ts))
            append(",\"level\":").append(Json.str(level.wire))
            append(",\"src\":").append(Json.str(SRC))
            append(",\"device\":").append(device?.let { Json.str(it) } ?: "null")
            append(",\"pc\":").append(pc?.let { Json.str(it) } ?: "null")
            append(",\"session\":").append(session?.let { Json.str(it) } ?: "null")
            append(",\"target\":").append(Json.str(finalTarget))
            append(",\"msg\":").append(Json.str(m))
            append(",\"fields\":").append(f).append('}')
        }
        fun size(line: String) = line.toByteArray(Charsets.UTF_8).size + 1
        var m = finalMsg
        var f = fieldsJson
        var line = assemble(m, f)
        var rounds = 0
        while (size(line) > MAX_LINE && rounds < 4) {
            val over = size(line) - MAX_LINE
            if (m.length > 200) {
                m = m.take((m.length - over - TRUNCATED.length - 8).coerceAtLeast(0))
                if (m.isNotEmpty() && m.last().isHighSurrogate()) m = m.dropLast(1)
                m += TRUNCATED
            } else {
                f = "{}"
            }
            line = assemble(m, f)
            rounds++
        }
        if (size(line) > MAX_LINE) line = assemble("[line too large]", "{}")
        val text = "$finalTarget: $m" + if (f == "{}") "" else " $f"
        return Built(line, text)
    }

    /** Writes everything queued and waits (up to [timeoutMs]) for the writer thread's in-flight batch. */
    fun flush(timeoutMs: Long = 2000) {
        try {
            val deadline = System.nanoTime() + TimeUnit.MILLISECONDS.toNanos(timeoutMs)
            synchronized(ioLock) {
                reportDropsLocked()
                drainLocked()
            }
            while (pending.get() > 0 && System.nanoTime() < deadline) Thread.sleep(5)
            synchronized(ioLock) { enforce(clock()) }
        } catch (_: Throwable) {
        }
    }

    /** Stops the writer thread after flushing; later lines are ignored. */
    fun close() {
        flush()
        closed = true
        writer?.interrupt()
    }

    private fun writerLoop() {
        try {
            while (!closed) {
                val first = queue.poll(1, TimeUnit.SECONDS)
                synchronized(ioLock) {
                    val batch = ArrayList<Entry>()
                    if (first != null) batch.add(first)
                    queue.drainTo(batch, 256)
                    if (batch.isNotEmpty()) writeBatchLocked(batch)
                    reportDropsLocked()
                }
            }
        } catch (_: InterruptedException) {
        } catch (_: Throwable) {
        }
    }

    private fun drainLocked() {
        val batch = ArrayList<Entry>()
        queue.drainTo(batch)
        if (batch.isNotEmpty()) writeBatchLocked(batch)
    }

    private fun reportDropsLocked() {
        val now = clock()
        if (now - lastDropReport < DROP_REPORT_MS) return
        lastDropReport = now
        val count = dropped.getAndSet(0)
        if (count == 0L) return
        val built = build(
            LogLevel.WARN, "BpLog", "log lines dropped", null, null,
            mapOf("dropped" to count, "windowMs" to DROP_REPORT_MS), null, now,
        )
        pending.incrementAndGet()
        writeBatchLocked(listOf(Entry(null, dayOf(now), built.line)))
    }

    private fun writeBatchLocked(batch: List<Entry>) {
        try {
            var bytes = 0L
            for ((key, group) in batch.groupBy { it.pc to it.day }) {
                val (pc, day) = key
                val dir = if (pc == null) File(root, "misc") else File(File(root, "pc"), pc)
                try {
                    dir.mkdirs()
                    val data = group.joinToString("") { it.line + "\n" }.toByteArray(Charsets.UTF_8)
                    FileOutputStream(File(dir, "$SRC-$day.jsonl"), true).use { it.write(data) }
                    bytes += data.size
                } catch (_: Throwable) {
                }
            }
            val day = batch.last().day
            bytesSinceSweep = if (bytesSinceSweep == Long.MAX_VALUE) bytes + SWEEP_BYTES + 1 else bytesSinceSweep + bytes
            if (bytesSinceSweep > SWEEP_BYTES || day != lastDay) {
                bytesSinceSweep = 0
                enforce(clock())
            }
            lastDay = day
        } finally {
            pending.addAndGet(-batch.size)
        }
    }

    /** Applies retention, the per-PC cap and the total cap (oldest day files first). */
    internal fun enforce(now: Long) {
        try {
            val cutoff = Instant.ofEpochMilli(now).atZone(ZoneOffset.UTC).toLocalDate()
                .minusDays(limits.retentionDays.toLong()).toString()
            val dirs = ArrayList<File>()
            File(root, "misc").takeIf { it.isDirectory }?.let { dirs.add(it) }
            File(root, "pc").listFiles()?.filter { it.isDirectory }?.let { dirs.addAll(it) }
            val kept = ArrayList<File>()
            for (dir in dirs) {
                val isPc = dir.parentFile?.name == "pc"
                val files = dayFiles(dir).toMutableList()
                val old = files.filter { dayKey(it) < cutoff }
                old.forEach { it.delete() }
                files.removeAll(old.toSet())
                if (isPc) {
                    var size = files.sumOf { it.length() }
                    while (size > limits.perPcBytes && files.isNotEmpty()) {
                        val oldest = files.removeAt(0)
                        size -= oldest.length()
                        oldest.delete()
                    }
                }
                kept.addAll(files)
                if (isPc && files.isEmpty()) dir.delete()
            }
            kept.sortWith(compareBy({ dayKey(it) }, { it.lastModified() }, { it.path }))
            var total = kept.sumOf { it.length() }
            for (file in kept) {
                if (total <= limits.totalBytes) break
                total -= file.length()
                file.delete()
            }
        } catch (_: Throwable) {
        }
    }

    private fun dayFiles(dir: File): List<File> =
        (dir.listFiles() ?: emptyArray()).filter { it.isFile && it.name.endsWith(".jsonl") }
            .sortedWith(compareBy({ dayKey(it) }, { it.name }))

    private fun dayKey(file: File): String =
        DAY.find(file.name)?.groupValues?.get(1)
            ?: Instant.ofEpochMilli(file.lastModified()).atZone(ZoneOffset.UTC).toLocalDate().toString()

    private fun dayOf(millis: Long): String =
        Instant.ofEpochMilli(millis).atZone(ZoneOffset.UTC).toLocalDate().toString()

    /**
     * Chains onto the previous default handler: an "uncaught" error line with the stack is written and
     * flushed on the crashing thread, then the previous handler runs.
     */
    fun installCrashHandler() {
        val previous = Thread.getDefaultUncaughtExceptionHandler()
        if (previous is CrashHandler && previous.log === this) return
        Thread.setDefaultUncaughtExceptionHandler(CrashHandler(this, previous))
    }

    private class CrashHandler(val log: BpLog, val previous: Thread.UncaughtExceptionHandler?) :
        Thread.UncaughtExceptionHandler {
        override fun uncaughtException(thread: Thread, error: Throwable) {
            log.write(
                LogLevel.ERROR, "uncaught", "uncaught exception on thread ${thread.name}", null, null,
                mapOf("thread" to thread.name), error, sync = true,
            )
            previous?.uncaughtException(thread, error)
        }
    }

    companion object {
        const val SRC = "app-android-core"
        private const val MAX_LINE = 8 * 1024
        private const val MAX_STACK = 4 * 1024
        private const val TRUNCATED = "...[truncated]"
        private const val DROP_REPORT_MS = 60_000L
        private const val SWEEP_BYTES = 64L * 1024
        private val SAFE_ID = Regex("[A-Za-z0-9_-]{1,64}")
        private val DAY = Regex("(\\d{4}-\\d{2}-\\d{2})\\.jsonl$")
        private val TS: DateTimeFormatter =
            DateTimeFormatter.ofPattern("yyyy-MM-dd'T'HH:mm:ss.SSS'Z'").withZone(ZoneOffset.UTC)

        @Volatile
        private var instance: BpLog? = null

        /** Starts logging into `filesDir/logs` and installs the crash handler. Safe to call again. */
        fun init(context: Context): BpLog = synchronized(this) {
            instance ?: BpLog(File(context.applicationContext.filesDir, "logs")).also {
                it.installCrashHandler()
                instance = it
            }
        }

        fun current(): BpLog? = instance

        /** Tests install their own instance (null removes it). */
        internal fun setCurrent(log: BpLog?) {
            instance = log
        }

        fun setDeviceId(id: String?) {
            instance?.setDevice(id)
        }

        fun secret(value: String) {
            instance?.registerSecret(value)
        }

        fun d(target: String, msg: String, pc: String? = null, fields: Map<String, Any?> = emptyMap()) {
            instance?.log(LogLevel.DEBUG, target, msg, pc, null, fields)
        }

        fun i(target: String, msg: String, pc: String? = null, fields: Map<String, Any?> = emptyMap()) {
            instance?.log(LogLevel.INFO, target, msg, pc, null, fields)
        }

        fun w(
            target: String,
            msg: String,
            pc: String? = null,
            fields: Map<String, Any?> = emptyMap(),
            error: Throwable? = null,
        ) {
            instance?.log(LogLevel.WARN, target, msg, pc, null, fields, error)
        }

        fun e(
            target: String,
            msg: String,
            pc: String? = null,
            fields: Map<String, Any?> = emptyMap(),
            error: Throwable? = null,
        ) {
            instance?.log(LogLevel.ERROR, target, msg, pc, null, fields, error)
        }

        private fun stackOf(error: Throwable): String {
            val writer = StringWriter()
            error.printStackTrace(PrintWriter(writer))
            return writer.toString()
        }

        private fun logcat(level: LogLevel, text: String) {
            android.util.Log.println(level.priority, "BubblePlay", text)
        }
    }
}

/** Minimal deterministic JSON writer (key order kept). */
internal object Json {
    fun write(value: Any?): String = StringBuilder().also { append(it, value) }.toString()

    fun str(s: String): String = StringBuilder().also { quote(it, s) }.toString()

    private fun append(out: StringBuilder, value: Any?) {
        when (value) {
            null -> out.append("null")
            is Boolean -> out.append(value)
            is Double -> if (value.isFinite()) out.append(value) else quote(out, value.toString())
            is Float -> if (value.isFinite()) out.append(value) else quote(out, value.toString())
            is Number -> out.append(value)
            is Map<*, *> -> {
                out.append('{')
                var first = true
                for ((k, v) in value) {
                    if (!first) out.append(',')
                    first = false
                    quote(out, k.toString())
                    out.append(':')
                    append(out, v)
                }
                out.append('}')
            }
            is Iterable<*> -> {
                out.append('[')
                var first = true
                for (v in value) {
                    if (!first) out.append(',')
                    first = false
                    append(out, v)
                }
                out.append(']')
            }
            else -> quote(out, value.toString())
        }
    }

    private fun quote(out: StringBuilder, s: String) {
        out.append('"')
        for (c in s) {
            when {
                c == '"' -> out.append("\\\"")
                c == '\\' -> out.append("\\\\")
                c == '\n' -> out.append("\\n")
                c == '\r' -> out.append("\\r")
                c == '\t' -> out.append("\\t")
                c < ' ' || c == ' ' || c == ' ' -> out.append("\\u%04x".format(c.code))
                else -> out.append(c)
            }
        }
        out.append('"')
    }
}
