package de.bubblebuffer.bubbleplay.log

import org.json.JSONObject
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNotNull
import org.junit.Assert.assertNull
import org.junit.Assert.assertSame
import org.junit.Assert.assertTrue
import org.junit.Test
import java.io.File
import java.time.Instant
import java.util.concurrent.atomic.AtomicLong
import kotlin.concurrent.thread

class BpLogTest {
    private val t0 = Instant.parse("2026-11-02T13:45:12.345Z").toEpochMilli()
    private val pcA = "77VH-7M56-8GJK-Y12J"
    private val pcB = "ABCD-EFGH-JKMN-PQRS"
    private val dayMs = 24L * 3600 * 1000

    @Test
    fun lineFollowsTheSpecAndLandsInThePcFile() = withTestLog({ t0 }) { log, dir ->
        log.setDevice("DEVI-CE12-3456-7890")
        log.log(LogLevel.INFO, "PairingClient", "pairing started", pcA, "sess-1", mapOf("relay" to "https://r.example", "n" to 3))
        log.flush()
        val file = File(dir, "pc/$pcA/app-android-core-2026-11-02.jsonl")
        assertTrue(file.isFile)
        val raw = file.readLines()
        assertEquals(1, raw.size)
        val line = JSONObject(raw[0])
        assertEquals(
            setOf("ts", "level", "src", "device", "pc", "session", "target", "msg", "fields"),
            line.keys().asSequence().toSet(),
        )
        assertTrue(raw[0].startsWith("{\"ts\":\"2026-11-02T13:45:12.345Z\",\"level\":\"info\",\"src\":\"app-android-core\","))
        assertEquals("2026-11-02T13:45:12.345Z", line.getString("ts"))
        assertEquals("info", line.getString("level"))
        assertEquals("app-android-core", line.getString("src"))
        assertEquals("DEVI-CE12-3456-7890", line.getString("device"))
        assertEquals(pcA, line.getString("pc"))
        assertEquals("sess-1", line.getString("session"))
        assertEquals("PairingClient", line.getString("target"))
        assertEquals("pairing started", line.getString("msg"))
        assertEquals("https://r.example", line.getJSONObject("fields").getString("relay"))
        assertEquals(3, line.getJSONObject("fields").getInt("n"))
    }

    @Test
    fun linesWithoutAPcGoToMiscAndABadPcIdIsNotAPath() = withTestLog({ t0 }) { log, dir ->
        log.log(LogLevel.INFO, "T", "no pc")
        log.log(LogLevel.INFO, "T", "evil pc", "../evil")
        log.flush()
        val misc = File(dir, "misc/app-android-core-2026-11-02.jsonl").readLines().map(::JSONObject)
        assertEquals(listOf("no pc", "evil pc"), misc.map { it.getString("msg") })
        assertTrue(misc.all { it.isNull("pc") && it.isNull("device") && it.isNull("session") })
        assertFalse(File(dir, "pc").exists())
        assertFalse(File(dir.parentFile, "evil").exists())
    }

    @Test
    fun belowTheMinimumLevelIsDropped() = withTestLog({ t0 }) { log, dir ->
        log.log(LogLevel.DEBUG, "T", "debug")
        log.log(LogLevel.TRACE, "T", "trace")
        log.log(LogLevel.WARN, "T", "warn")
        log.log(LogLevel.ERROR, "T", "error")
        log.flush()
        assertEquals(listOf("warn", "error"), dir.logLines().map { it.getString("level") })
    }

    @Test
    fun errorsCarryTheClassTheMessageAndABoundedStack() = withTestLog({ t0 }) { log, dir ->
        fun deep(n: Int): Nothing = if (n == 0) throw IllegalStateException("bad state") else deep(n - 1)
        val error = try { deep(400) } catch (e: IllegalStateException) { e }
        log.log(LogLevel.WARN, "T", "failed", fields = mapOf("a" to 1), error = error)
        log.flush()
        val fields = dir.logLines().single().getJSONObject("fields")
        assertEquals("java.lang.IllegalStateException: bad state", fields.getString("error"))
        assertTrue(fields.getString("stack").length <= 4096)
        assertTrue(fields.getString("stack").startsWith("java.lang.IllegalStateException: bad state"))
        assertEquals(1, fields.getInt("a"))
    }

    @Test
    fun aLongMessageIsTruncatedToTheLineLimit() = withTestLog({ t0 }) { log, dir ->
        log.log(LogLevel.INFO, "T", "x ".repeat(20_000), pcA, fields = mapOf("note" to "y ".repeat(5_000)))
        log.flush()
        val raw = dir.jsonlFiles().single().readLines().single()
        assertTrue(raw.toByteArray().size + 1 <= 8192)
        assertTrue(JSONObject(raw).getString("msg").endsWith("...[truncated]"))
    }

    @Test
    fun registeredSecretsAndPatternsNeverReachTheFileOrLogcat() {
        val mirrored = mutableListOf<String>()
        withTestLog({ t0 }, mirror = { _, text -> mirrored += text }) { log, dir ->
            val secret = "hunter2-correct-horse"
            log.registerSecret(secret)
            log.log(
                LogLevel.WARN, "T", "login with $secret and password=other1 at /x?token=tok123&a=1",
                pcA, fields = mapOf("note" to "seen $secret", "passphrase" to "p4ss", "n" to 5),
                error = IllegalArgumentException("bad: $secret"),
            )
            log.flush()
            val text = dir.allText()
            for (needle in listOf(secret, "other1", "tok123", "p4ss")) {
                assertFalse("file contains $needle", text.contains(needle))
                assertTrue("logcat contains $needle", mirrored.none { it.contains(needle) })
            }
            val line = dir.logLines().single()
            assertEquals(Redact.MASK, line.getJSONObject("fields").getString("passphrase"))
            assertEquals(5, line.getJSONObject("fields").getInt("n"))
            // The logcat mirror is the same redacted message.
            assertEquals(1, mirrored.size)
            assertTrue(mirrored[0].startsWith("T: ${line.getString("msg")}"))
        }
    }

    @Test
    fun aFailingRedactionWritesAPlaceholderLine() = withTestLog({ t0 }) { log, dir ->
        val broken = object : AbstractMap<String, Any?>() {
            override val entries: Set<Map.Entry<String, Any?>> get() = throw IllegalStateException("boom")
        }
        log.log(LogLevel.INFO, "T", "secret text", fields = broken)
        log.flush()
        val line = dir.logLines().single()
        assertEquals("[redaction failed]", line.getString("msg"))
        assertEquals(0, line.getJSONObject("fields").length())
    }

    @Test
    fun theOldestDayFilesGoFirstWhenAPcExceedsItsCap() {
        var now = t0
        val limits = LogLimits(perPcBytes = 1500, totalBytes = 1_000_000, retentionDays = 30)
        withTestLog({ now }, limits = limits) { log, dir ->
            repeat(6) { day ->
                now = t0 + day * dayMs
                repeat(4) { log.log(LogLevel.INFO, "T", "day $day line $it ".padEnd(60, '.'), pcA) }
                log.flush()
            }
            val files = File(dir, "pc/$pcA").listFiles()!!.sortedBy { it.name }
            assertTrue(files.sumOf { it.length() } <= 1500)
            assertTrue(files.isNotEmpty())
            assertEquals("app-android-core-2026-11-07.jsonl", files.last().name)
            assertFalse(File(dir, "pc/$pcA/app-android-core-2026-11-02.jsonl").exists())
        }
    }

    @Test
    fun theTotalCapDeletesTheOldestFilesAcrossPcsAndMisc() {
        var now = t0
        val limits = LogLimits(perPcBytes = 1_000_000, totalBytes = 2500, retentionDays = 30)
        withTestLog({ now }, limits = limits) { log, dir ->
            repeat(5) { day ->
                now = t0 + day * dayMs
                for (pc in listOf(pcA, pcB, null)) repeat(3) { log.log(LogLevel.INFO, "T", "d$day $it".padEnd(80, '.'), pc) }
                log.flush()
            }
            val files = dir.jsonlFiles()
            assertTrue(files.sumOf { it.length() } <= 2500)
            assertTrue(files.any { it.name.endsWith("2026-11-06.jsonl") })
            assertTrue(files.none { it.name.endsWith("2026-11-02.jsonl") })
        }
    }

    @Test
    fun filesOlderThanTheRetentionAreDeletedWhoeverWroteThem() {
        withTestLog({ t0 }) { log, dir ->
            val old = File(dir, "pc/$pcA/app-android-2026-09-01.jsonl").apply { parentFile.mkdirs(); writeText("{}\n") }
            val dart = File(dir, "pc/$pcA/app-android-2026-10-30.jsonl").apply { writeText("{}\n") }
            val oldOnly = File(dir, "pc/$pcB/app-android-core-2026-08-01.jsonl").apply { parentFile.mkdirs(); writeText("{}\n") }
            log.log(LogLevel.INFO, "T", "now", pcA)
            log.flush()
            assertFalse(old.exists())
            assertTrue(dart.exists())
            assertFalse(oldOnly.exists())
            assertFalse(File(dir, "pc/$pcB").exists())
        }
    }

    @Test
    fun aFullQueueDropsTheOldestLinesAndReportsTheCount() {
        val now = AtomicLong(t0)
        withTestLog({ now.get() }, capacity = 3, startWriter = false) { log, dir ->
            repeat(10) { log.log(LogLevel.INFO, "T", "m$it", pcA) }
            now.addAndGet(61_000)
            log.flush()
            val lines = dir.logLines()
            val kept = lines.filter { it.getString("target") == "T" }.map { it.getString("msg") }
            assertEquals(listOf("m7", "m8", "m9"), kept)
            val report = lines.single { it.getString("target") == "BpLog" }
            assertEquals("warn", report.getString("level"))
            assertEquals(7, report.getJSONObject("fields").getInt("dropped"))
        }
    }

    @Test
    fun manyThreadsLoseNothingWithinTheQueue() = withTestLog({ t0 }) { log, dir ->
        val workers = (0 until 4).map { w -> thread { repeat(200) { log.log(LogLevel.INFO, "T", "w$w-$it", pcA) } } }
        workers.forEach { it.join() }
        log.flush()
        assertEquals(800, dir.logLines().size)
    }

    @Test
    fun theCrashHandlerWritesAnUncaughtLineThenDelegates() {
        val original = Thread.getDefaultUncaughtExceptionHandler()
        val delegated = mutableListOf<Throwable>()
        Thread.setDefaultUncaughtExceptionHandler { _, e -> delegated += e }
        try {
            withTestLog({ t0 }) { log, dir ->
                log.installCrashHandler()
                log.installCrashHandler() // a second install must not chain onto itself
                val handler = Thread.getDefaultUncaughtExceptionHandler()
                assertNotNull(handler)
                val boom = RuntimeException("exploded password=abc123")
                handler!!.uncaughtException(Thread.currentThread(), boom)
                // No flush: the crash line is written on the crashing thread.
                val line = dir.logLines().single()
                assertEquals("error", line.getString("level"))
                assertEquals("uncaught", line.getString("target"))
                val fields = line.getJSONObject("fields")
                assertTrue(fields.getString("stack").contains("RuntimeException"))
                assertTrue(fields.getString("stack").contains("BpLogTest"))
                assertFalse(dir.allText().contains("abc123"))
                assertEquals(listOf<Throwable>(boom), delegated)
                assertEquals(Thread.currentThread().name, fields.getString("thread"))
            }
        } finally {
            Thread.setDefaultUncaughtExceptionHandler(original)
        }
    }

    @Test
    fun theStaticHelpersDoNothingWithoutAnInstalledLog() {
        BpLog.setCurrent(null)
        BpLog.i("T", "ignored")
        BpLog.w("T", "ignored", pcA, mapOf("a" to 1), RuntimeException())
        assertNull(BpLog.current())
    }

    @Test
    fun theStaticHelpersWriteThroughTheCurrentLog() = withTestLog({ t0 }) { log, dir ->
        assertSame(log, BpLog.current())
        BpLog.setDeviceId("DEVI-CE12-3456-7890")
        BpLog.i("T", "info line", pcA, mapOf("k" to "v"))
        BpLog.e("T", "error line", error = RuntimeException("x"))
        log.flush()
        val lines = dir.logLines()
        assertEquals(2, lines.size)
        assertTrue(lines.all { it.getString("device") == "DEVI-CE12-3456-7890" })
    }
}
