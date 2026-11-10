package de.bubblebuffer.bubbleplay.log

import org.json.JSONObject
import java.io.File
import java.nio.file.Files

/**
 * A wall clock that never returns the same millisecond twice, so every line a test writes has its own `ts`.
 * Lines sharing a `ts` in different files sort by file path, not by write order (see LogCursor), which made
 * tests that write to several files back to back depend on whether two writes fell into one millisecond.
 */
internal fun strictlyIncreasingClock(): () -> Long {
    val last = java.util.concurrent.atomic.AtomicLong(Long.MIN_VALUE)
    return { last.updateAndGet { prev -> maxOf(prev + 1, System.currentTimeMillis()) } }
}

/** Runs [block] with a [BpLog] on a temp directory installed as the current log; cleans up after. */
internal fun <T> withTestLog(
    clock: () -> Long = strictlyIncreasingClock(),
    mirror: (LogLevel, String) -> Unit = { _, _ -> },
    limits: LogLimits = LogLimits(),
    capacity: Int = 1024,
    startWriter: Boolean = true,
    block: (BpLog, File) -> T,
): T {
    val dir = Files.createTempDirectory("bplog").toFile()
    val log = BpLog(dir, clock, mirror, capacity, limits, LogLevel.INFO, startWriter)
    BpLog.setCurrent(log)
    try {
        return block(log, dir)
    } finally {
        BpLog.setCurrent(null)
        log.close()
        dir.deleteRecursively()
    }
}

internal fun File.jsonlFiles(): List<File> = walkTopDown().filter { it.isFile && it.name.endsWith(".jsonl") }.toList()

internal fun File.logLines(): List<JSONObject> =
    jsonlFiles().sortedBy { it.path }.flatMap { file -> file.readLines().filter { it.isNotBlank() }.map(::JSONObject) }

internal fun File.allText(): String = jsonlFiles().joinToString("\n") { it.readText() }
