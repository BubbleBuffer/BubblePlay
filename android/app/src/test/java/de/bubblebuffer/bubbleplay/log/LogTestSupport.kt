package de.bubblebuffer.bubbleplay.log

import org.json.JSONObject
import java.io.File
import java.nio.file.Files

/** Runs [block] with a [BpLog] on a temp directory installed as the current log; cleans up after. */
internal fun <T> withTestLog(
    clock: () -> Long = System::currentTimeMillis,
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
