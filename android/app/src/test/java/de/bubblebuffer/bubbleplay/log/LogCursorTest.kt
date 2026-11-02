package de.bubblebuffer.bubbleplay.log

import org.junit.Assert.assertEquals
import org.junit.Assert.assertTrue
import org.junit.Test
import java.io.File
import java.util.concurrent.atomic.AtomicLong

class LogCursorTest {
    private val t0 = java.time.Instant.parse("2026-11-02T13:45:12.345Z").toEpochMilli()

    private fun line(ts: String, pc: String?, msg: String) =
        "{\"ts\":\"$ts\",\"level\":\"info\",\"src\":\"app-android-core\",\"device\":null,\"pc\":" +
            (pc?.let { "\"$it\"" } ?: "null") + ",\"session\":null,\"target\":\"T\",\"msg\":\"$msg\",\"fields\":{}}"

    @Test
    fun cursorIsFixedWidthAndCompareByteWise() {
        val a = LogCursor.make("2026-11-02T13:45:12.345Z", 9)
        val b = LogCursor.make("2026-11-02T13:45:12.345Z", 10)
        val c = LogCursor.make("2026-11-02T13:45:12.346Z", 0)
        assertEquals("2026-11-02T13:45:12.345Z#000009", a)
        assertTrue(a.length == b.length && b.length == c.length)
        assertTrue(a < b && b < c)
        assertTrue(a.matches(Regex("[0-9A-Za-z#:._-]{1,64}")))
    }

    @Test
    fun linesInOneMillisecondGetIncreasingCountersAcrossFiles() = withTestLog({ t0 }) { log, dir ->
        log.log(LogLevel.INFO, "T", "one", "pc-1")
        log.log(LogLevel.INFO, "T", "two")
        log.log(LogLevel.INFO, "T", "three", "pc-1")
        log.flush()
        val lines = LogCursor.readAfter(dir, "", "pc-1", 100, 1 shl 20)
        assertEquals(3, lines.size)
        assertEquals(lines.map { it.cursor }, lines.map { it.cursor }.sorted())
        assertEquals(3, lines.map { it.cursor }.toSet().size)
        assertTrue(lines.all { it.cursor.startsWith("2026-11-02T13:45:12.345Z#") })
        // Reading after a cursor gives exactly the later lines.
        val rest = LogCursor.readAfter(dir, lines[0].cursor, "pc-1", 100, 1 shl 20)
        assertEquals(lines.drop(1).map { it.cursor }, rest.map { it.cursor })
    }

    @Test
    fun filtersByPcAndKeepsLinesWithoutPc() = withTestLog({ t0 }) { log, dir ->
        val clock = AtomicLong(t0)
        log.log(LogLevel.INFO, "T", "mine", "pc-1")
        log.log(LogLevel.INFO, "T", "other", "pc-2")
        log.log(LogLevel.INFO, "T", "nopc")
        log.flush()
        val msgs = LogCursor.readAfter(dir, "", "pc-1", 100, 1 shl 20).map { org.json.JSONObject(it.line).getString("msg") }
        assertEquals(setOf("mine", "nopc"), msgs.toSet())
        assertEquals(2, msgs.size)
        assertTrue(clock.get() == t0)
    }

    @Test
    fun limitsLinesAndBytesButAlwaysReturnsOneLine() {
        val dir = java.nio.file.Files.createTempDirectory("cur").toFile()
        try {
            val file = File(dir, "misc/app-android-core-2026-11-02.jsonl")
            file.parentFile.mkdirs()
            val text = (0 until 10).joinToString("") { line("2026-11-02T13:45:12.%03dZ".format(it), null, "m$it") + "\n" }
            file.writeText(text)
            assertEquals(4, LogCursor.readAfter(dir, "", "pc", 4, 1 shl 20).size)
            val size = line("2026-11-02T13:45:12.000Z", null, "m0").toByteArray().size
            assertEquals(2, LogCursor.readAfter(dir, "", "pc", 100, size * 2 + 1).size)
            assertEquals(1, LogCursor.readAfter(dir, "", "pc", 100, 1).size)
        } finally {
            dir.deleteRecursively()
        }
    }

    @Test
    fun aHalfWrittenLastLineAndGarbageAreIgnored() {
        val dir = java.nio.file.Files.createTempDirectory("cur").toFile()
        try {
            val file = File(dir, "misc/app-android-core-2026-11-02.jsonl")
            file.parentFile.mkdirs()
            file.writeText(
                line("2026-11-02T13:45:12.001Z", null, "ok") + "\nnot json\n" +
                    line("2026-11-02T13:45:12.002Z", null, "half"),
            )
            val read = LogCursor.readAfter(dir, "", "pc", 100, 1 shl 20)
            assertEquals(1, read.size)
            assertEquals("2026-11-02T13:45:12.001Z#000000", read[0].cursor)
        } finally {
            dir.deleteRecursively()
        }
    }

    @Test
    fun missingRootGivesNothing() {
        assertEquals(0, LogCursor.readAfter(File("/nonexistent-bp-dir"), "", "pc", 10, 100).size)
    }
}
