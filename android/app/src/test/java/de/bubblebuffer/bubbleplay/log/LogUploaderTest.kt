package de.bubblebuffer.bubbleplay.log

import de.bubblebuffer.bubbleplay.Fixtures
import org.json.JSONObject
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Test
import java.io.File
import java.time.Instant
import java.util.concurrent.atomic.AtomicLong

class LogUploaderTest {
    private val t0 = Instant.parse("2026-11-02T13:45:12.000Z").toEpochMilli()
    private val pc = "pc-1"
    private val contract = Fixtures.json("client-log-upload-contract.json")

    private fun fixture(name: String): JSONObject {
        val arr = contract.getJSONArray("messages")
        for (i in 0 until arr.length()) {
            if (arr.getJSONObject(i).getString("name") == name) return arr.getJSONObject(i).getJSONObject("json")
        }
        error("no fixture $name")
    }

    private class Rig(val log: BpLog, val dir: File, val clock: AtomicLong) {
        val transport = FakeTransport()
        val scheduler = ManualScheduler(clock)
        val uploader = LogUploader(transport, log, "pc-1", clock::get, scheduler)

        fun state(cursor: String? = null, limits: String = "") {
            val cursors = if (cursor == null) "{}" else "{\"app-android-core\":\"$cursor\"}"
            val lim = limits.ifEmpty {
                "{\"maxBatchBytes\":16384,\"maxBatchLines\":100,\"connectionBudgetBytes\":8388608,\"minBatchIntervalMs\":500}"
            }
            transport.deliver("{\"type\":\"logs-state\",\"v\":1,\"cursors\":$cursors,\"limits\":$lim}")
            scheduler.runDue()
        }

        fun ack(batch: JSONObject, status: String = "ok", cursor: String? = null, retryAfterMs: Long = 0, reason: String? = null) {
            val msg = JSONObject().put("type", "logs-ack").put("v", 1).put("stream", "app-android-core")
                .put("id", batch.getLong("id")).put("status", status)
                .put("cursor", cursor ?: batch.getString("to")).put("dropped", 0).put("retryAfterMs", retryAfterMs)
            if (reason != null) msg.put("reason", reason)
            transport.deliver(msg)
            scheduler.runDue()
        }

        fun write(n: Int, msg: String = "l", pcId: String? = "pc-1") {
            repeat(n) {
                clock.incrementAndGet()
                log.log(LogLevel.INFO, "T", "$msg $it", pcId)
            }
            log.flush()
            scheduler.runDue()
        }

        fun storedLines(): List<String> =
            dir.jsonlFiles().sortedBy { it.path }.flatMap { f -> f.readLines().filter { it.isNotBlank() } }

        fun lines(batch: JSONObject): List<String> =
            (0 until batch.getJSONArray("lines").length()).map { batch.getJSONArray("lines").getString(it) }
    }

    private fun rig(block: (Rig) -> Unit) {
        val clock = AtomicLong(t0)
        withTestLog({ clock.get() }) { log, dir ->
            val r = Rig(log, dir, clock)
            try {
                block(r)
            } finally {
                r.uploader.close()
            }
        }
    }

    @Test
    fun nothingIsSentBeforeLogsState() = rig { r ->
        r.write(3)
        r.uploader.start()
        r.scheduler.advance(5000)
        assertEquals(0, r.transport.sent.size)
    }

    @Test
    fun historyIsSentAsStoredWithFixedWidthCursors() = rig { r ->
        r.write(3)
        r.write(1, "other", "pc-2")
        r.write(1, "nopc", null)
        r.uploader.start()
        r.clock.addAndGet(10_000)
        r.state()
        assertEquals(1, r.transport.sent.size)
        val batch = r.transport.sent[0]
        assertEquals(setOf("type", "v", "stream", "id", "from", "to", "live", "lines"), batch.keys().asSequence().toSet())
        assertEquals(fixture("logs-batch").keys().asSequence().toSet(), batch.keys().asSequence().toSet())
        assertEquals("logs-batch", batch.getString("type"))
        assertEquals(1, batch.getInt("v"))
        assertEquals("app-android-core", batch.getString("stream"))
        assertEquals("", batch.getString("from"))
        assertFalse(batch.getBoolean("live"))
        val lines = r.lines(batch)
        assertEquals(4, lines.size)
        assertTrue(lines.none { "other 0" in it })
        val stored = r.storedLines().toSet()
        assertTrue(lines.all { it in stored })
        val to = batch.getString("to")
        assertEquals(31, to.length)
        assertTrue(to.matches(Regex("[0-9A-Za-z#:._-]{1,64}")))
        assertTrue(to.contains("#"))
    }

    @Test
    fun resumesAfterTheCursorFromLogsState() = rig { r ->
        r.write(5)
        r.uploader.start()
        r.clock.addAndGet(10_000)
        val all = r.log.readAfter("", pc, 100, 1 shl 20)
        r.state(all[1].cursor)
        val batch = r.transport.sent.single()
        assertEquals(all[1].cursor, batch.getString("from"))
        assertEquals(all.drop(2).map { it.line }, r.lines(batch))
        assertEquals(all.last().cursor, batch.getString("to"))
    }

    @Test
    fun fixtureLogsStateCursorSkipsEarlierLines() {
        val cursor = fixture("logs-state").getJSONObject("cursors").getString("app-android-core")
        val clock = AtomicLong(Instant.parse("2026-11-02T13:45:11.000Z").toEpochMilli())
        withTestLog({ clock.get() }) { log, dir ->
            val r = Rig(log, dir, clock)
            try {
                r.write(10) // before the fixture cursor
                clock.set(Instant.parse("2026-11-02T13:45:20.000Z").toEpochMilli())
                r.write(2, "later")
                r.uploader.start()
                r.transport.deliver(fixture("logs-state"))
                r.scheduler.runDue()
                val batch = r.transport.sent.single()
                assertEquals(cursor, batch.getString("from"))
                assertEquals(2, r.lines(batch).size)
                assertTrue(r.lines(batch).all { "later" in it })
            } finally {
                r.uploader.close()
            }
        }
    }

    @Test
    fun fixtureAcksAreUnderstood() = rig { r ->
        // ok, rate-limited, budget and rejected fixture acks, each with the id of the batch in flight
        for (name in listOf("logs-ack-ok", "logs-ack-rate-limited", "logs-ack-budget", "logs-ack-rejected")) {
            val ack = fixture(name)
            assertEquals("logs-ack", ack.getString("type"))
            assertEquals(1, ack.getInt("v"))
            assertTrue(ack.getString("status") in setOf("ok", "rate-limited", "budget", "rejected"))
        }
        r.write(2)
        r.uploader.start()
        r.clock.addAndGet(5000)
        r.state()
        val first = r.transport.sent[0]
        val ok = JSONObject(fixture("logs-ack-ok").toString()).put("id", first.getLong("id")).put("cursor", first.getString("to"))
        r.transport.deliver(ok)
        r.scheduler.runDue()
        r.write(1, "next")
        r.scheduler.advance(600)
        assertEquals(2, r.transport.sent.size)
        val second = r.transport.sent[1]
        val limited = JSONObject(fixture("logs-ack-rate-limited").toString()).put("id", second.getLong("id")).put("cursor", first.getString("to"))
        r.transport.deliver(limited)
        r.scheduler.advance(1999)
        assertEquals(2, r.transport.sent.size)
        r.scheduler.advance(1)
        assertEquals(3, r.transport.sent.size)
        val rejected = JSONObject(fixture("logs-ack-rejected").toString()).put("id", second.getLong("id")).put("cursor", first.getString("to"))
        r.transport.deliver(rejected)
        r.scheduler.advance(5000)
        assertEquals(3, r.transport.sent.size)
        r.write(1, "after")
        r.scheduler.advance(600)
        assertEquals(4, r.transport.sent.size)
        val fourth = r.transport.sent[3]
        val budget = JSONObject(fixture("logs-ack-budget").toString()).put("id", fourth.getLong("id")).put("cursor", first.getString("to"))
        r.transport.deliver(budget)
        r.write(1, "never")
        r.scheduler.advance(60_000)
        assertEquals(4, r.transport.sent.size)
    }

    @Test
    fun oneBatchInFlightUntilItsAck() = rig { r ->
        r.write(250)
        r.uploader.start()
        r.clock.addAndGet(10_000)
        r.state()
        assertEquals(1, r.transport.sent.size)
        assertEquals(100, r.lines(r.transport.sent[0]).size)
        r.scheduler.advance(5000)
        assertEquals(1, r.transport.sent.size)
        r.ack(r.transport.sent[0])
        assertEquals(2, r.transport.sent.size) // the interval has long passed
    }

    @Test
    fun minBatchIntervalSpacesBatches() = rig { r ->
        r.write(250)
        r.uploader.start()
        r.clock.addAndGet(10_000)
        r.state()
        val first = r.transport.sent[0]
        r.scheduler.advance(100)
        r.ack(first)
        assertEquals(1, r.transport.sent.size)
        r.scheduler.advance(399)
        assertEquals(1, r.transport.sent.size)
        r.scheduler.advance(1)
        assertEquals(2, r.transport.sent.size)
        val second = r.transport.sent[1]
        assertEquals(first.getString("to"), second.getString("from"))
        assertEquals(first.getLong("id") + 1, second.getLong("id"))
        assertEquals(100, r.lines(second).size)
    }

    @Test
    fun afterAckNothingOldIsSentAgain() = rig { r ->
        r.write(3)
        r.uploader.start()
        r.clock.addAndGet(10_000)
        r.state()
        r.ack(r.transport.sent[0])
        r.scheduler.advance(5000)
        assertEquals(1, r.transport.sent.size)
    }

    @Test
    fun liveLinesAreSentAsLiveAfterHistory() = rig { r ->
        r.write(2)
        r.uploader.start()
        r.clock.addAndGet(1000)
        r.state()
        assertFalse(r.transport.sent[0].getBoolean("live"))
        r.ack(r.transport.sent[0])
        r.scheduler.advance(1000)
        r.write(3, "fresh")
        r.scheduler.advance(600)
        assertEquals(2, r.transport.sent.size)
        val live = r.transport.sent[1]
        assertTrue(live.getBoolean("live"))
        assertEquals(3, r.lines(live).size)
        assertTrue(r.lines(live).all { "fresh" in it })
    }

    @Test
    fun liveLinesAreCoalescedPerInterval() = rig { r ->
        r.uploader.start()
        r.state()
        r.write(1, "a")
        assertEquals(1, r.transport.sent.size)
        r.ack(r.transport.sent[0])
        r.write(1, "b")
        r.write(1, "c")
        r.write(1, "d")
        assertEquals(1, r.transport.sent.size)
        r.scheduler.advance(600)
        assertEquals(2, r.transport.sent.size)
        assertEquals(3, r.lines(r.transport.sent[1]).size)
    }

    @Test
    fun hundredLinesPerBatch() = rig { r ->
        r.write(101)
        r.uploader.start()
        r.clock.addAndGet(5000)
        r.state()
        assertEquals(100, r.lines(r.transport.sent[0]).size)
    }

    @Test
    fun sixteenKiBPerBatch() = rig { r ->
        val big = "x ".repeat(1500) // 3000 characters, not a blob
        repeat(12) {
            r.clock.incrementAndGet()
            r.log.log(LogLevel.INFO, "T", "$big$it", pc)
        }
        r.log.flush()
        r.uploader.start()
        r.clock.addAndGet(5000)
        r.state()
        repeat(8) {
            val last = r.transport.sent.last()
            val size = r.lines(last).sumOf { it.toByteArray().size + 1 }
            assertTrue(size <= 16 * 1024)
            r.ack(last)
            r.scheduler.advance(600)
        }
        assertTrue(r.transport.sent.size > 1)
        assertEquals(12, r.transport.sent.sumOf { r.lines(it).size })
    }

    @Test
    fun rateLimitedResendsTheSameBatchAfterRetryAfter() = rig { r ->
        r.write(3)
        r.uploader.start()
        r.clock.addAndGet(5000)
        r.state()
        val first = r.transport.sent[0]
        r.ack(first, "rate-limited", cursor = "", retryAfterMs = 2000)
        assertEquals(1, r.transport.sent.size)
        r.scheduler.advance(1999)
        assertEquals(1, r.transport.sent.size)
        r.scheduler.advance(1)
        assertEquals(2, r.transport.sent.size)
        assertEquals(first.getLong("id"), r.transport.sent[1].getLong("id"))
        assertEquals(first.getJSONArray("lines").toString(), r.transport.sent[1].getJSONArray("lines").toString())
        assertEquals(first.getString("from"), r.transport.sent[1].getString("from"))
        r.ack(r.transport.sent[1])
        r.scheduler.advance(5000)
        assertEquals(2, r.transport.sent.size)
    }

    @Test
    fun budgetStopsUntilANewConnection() = rig { r ->
        r.write(3)
        r.uploader.start()
        r.clock.addAndGet(5000)
        r.state()
        r.ack(r.transport.sent[0], "budget")
        r.write(5, "more")
        r.scheduler.advance(60_000)
        assertEquals(1, r.transport.sent.size)
    }

    @Test
    fun connectionBudgetFromLogsStateIsRespectedLocally() = rig { r ->
        r.write(3)
        r.uploader.start()
        r.clock.addAndGet(5000)
        r.state(limits = "{\"maxBatchBytes\":16384,\"maxBatchLines\":100,\"connectionBudgetBytes\":100,\"minBatchIntervalMs\":500}")
        assertEquals(0, r.transport.sent.size)
    }

    @Test
    fun rejectedBatchIsDroppedAndNotRetried() = rig { r ->
        r.write(3)
        r.uploader.start()
        r.clock.addAndGet(5000)
        r.state()
        val first = r.transport.sent[0]
        r.ack(first, "rejected", cursor = "", reason = "batch too large")
        r.scheduler.advance(5000)
        assertEquals(1, r.transport.sent.size)
        r.write(2, "after")
        r.scheduler.advance(600)
        assertEquals(2, r.transport.sent.size)
        assertEquals(first.getString("to"), r.transport.sent[1].getString("from"))
        assertTrue(r.lines(r.transport.sent[1]).all { "after" in it })
    }

    @Test
    fun ackOkResumesFromTheHostsCursor() = rig { r ->
        r.write(6)
        r.uploader.start()
        r.clock.addAndGet(5000)
        r.state(limits = "{\"maxBatchBytes\":16384,\"maxBatchLines\":3,\"connectionBudgetBytes\":8388608,\"minBatchIntervalMs\":500}")
        val first = r.transport.sent[0]
        assertEquals(3, r.lines(first).size)
        val all = r.log.readAfter("", pc, 100, 1 shl 20)
        r.ack(first, cursor = all[0].cursor) // the host stored only the first line
        r.scheduler.advance(600)
        val second = r.transport.sent[1]
        assertEquals(all[0].cursor, second.getString("from"))
        assertEquals(all.subList(1, 4).map { it.line }, r.lines(second))
    }

    @Test
    fun unknownIdsStreamsAndVersionsAreIgnored() = rig { r ->
        r.write(2)
        r.uploader.start()
        r.clock.addAndGet(5000)
        r.state()
        val first = r.transport.sent[0]
        fun ack(id: Long, stream: String, v: Int) = r.transport.deliver(
            JSONObject().put("type", "logs-ack").put("v", v).put("stream", stream).put("id", id)
                .put("status", "ok").put("cursor", first.getString("to")),
        )
        ack(999, "app-android-core", 1)
        ack(first.getLong("id"), "browser", 1)
        ack(first.getLong("id"), "app-android-core", 2)
        r.scheduler.runDue()
        r.write(1, "next")
        r.scheduler.advance(5000)
        assertEquals(1, r.transport.sent.size) // still waiting for the real ack
    }

    @Test
    fun bufferedBytesGateWaitsUntilTheBufferDrains() = rig { r ->
        r.write(2)
        r.uploader.start()
        r.clock.addAndGet(5000)
        r.transport.bufferedBytes = 16 * 1024 + 1
        r.state()
        r.scheduler.advance(3000)
        assertEquals(0, r.transport.sent.size)
        r.transport.bufferedBytes = 16 * 1024
        r.scheduler.advance(150)
        assertEquals(1, r.transport.sent.size)
    }

    @Test
    fun recentInputHoldsBackUntilQuietFor250Ms() = rig { r ->
        r.write(2)
        r.uploader.start()
        r.clock.addAndGet(5000)
        r.transport.sinceLastInputMs = 100
        r.state()
        assertEquals(0, r.transport.sent.size)
        r.transport.sinceLastInputMs = 249
        r.scheduler.advance(10)
        assertEquals(0, r.transport.sent.size)
        r.transport.sinceLastInputMs = 250
        r.scheduler.advance(300)
        assertEquals(1, r.transport.sent.size)
    }

    @Test
    fun aTransportThatCannotSendIsRetriedLater() = rig { r ->
        r.write(2)
        r.uploader.start()
        r.clock.addAndGet(5000)
        r.transport.accept = false
        r.state()
        assertEquals(0, r.transport.sent.size)
        r.transport.accept = true
        r.scheduler.advance(2000)
        assertEquals(1, r.transport.sent.size)
        assertEquals(2, r.lines(r.transport.sent[0]).size)
    }

    @Test
    fun neverThrowsIntoTheCaller() = rig { r ->
        r.write(2)
        r.transport.throwOnSend = true
        r.uploader.start()
        r.clock.addAndGet(5000)
        r.state()
        r.transport.deliver("{\"type\":\"logs-ack\"}")
        r.transport.deliver("{\"type\":\"logs-state\",\"v\":1,\"cursors\":\"nope\"}")
        r.scheduler.advance(10_000)
        r.uploader.close()
        r.uploader.close()
        r.uploader.start()
    }

    @Test
    fun redactionStaysAsStored() = rig { r ->
        r.log.registerSecret("hunter2-literal")
        r.clock.incrementAndGet()
        r.log.log(
            LogLevel.INFO, "T", "login with hunter2-literal and token=abc123", pc,
            fields = mapOf("password" to "p4ss", "n" to 1),
        )
        r.log.flush()
        r.uploader.start()
        r.clock.addAndGet(5000)
        r.state()
        val lines = r.lines(r.transport.sent[0])
        assertEquals(1, lines.size)
        assertEquals(r.storedLines(), lines)
        val text = r.transport.sent[0].toString()
        assertFalse("hunter2-literal" in text)
        assertFalse("p4ss" in text)
        assertFalse("abc123" in text)
        assertTrue("[redacted]" in lines[0])
    }

    @Test
    fun closeStopsSendingAndRemovesListeners() = rig { r ->
        r.write(2)
        r.uploader.start()
        r.clock.addAndGet(5000)
        r.uploader.close()
        assertNull(r.transport.current)
        r.write(3, "late")
        r.scheduler.advance(10_000)
        assertEquals(0, r.transport.sent.size)
        assertTrue(r.scheduler.shutDown)
    }

    @Test
    fun closeAfterStateStopsFurtherBatches() = rig { r ->
        r.write(250)
        r.uploader.start()
        r.clock.addAndGet(5000)
        r.state()
        r.uploader.close()
        r.scheduler.advance(10_000)
        assertEquals(1, r.transport.sent.size)
    }

    @Test
    fun realWorkerThreadUploadsAndCloses() {
        withTestLog { log, _ ->
            log.log(LogLevel.INFO, "T", "hello", "pc-1")
            log.flush()
            val transport = FakeTransport()
            val up = LogUploader(transport, log, "pc-1")
            up.start()
            transport.deliver(
                "{\"type\":\"logs-state\",\"v\":1,\"cursors\":{},\"limits\":{\"maxBatchBytes\":16384," +
                    "\"maxBatchLines\":100,\"connectionBudgetBytes\":8388608,\"minBatchIntervalMs\":0}}",
            )
            val deadline = System.currentTimeMillis() + 5000
            while (transport.sentCount() == 0 && System.currentTimeMillis() < deadline) Thread.sleep(10)
            up.close()
            assertEquals(1, transport.sentCount())
        }
    }
}
