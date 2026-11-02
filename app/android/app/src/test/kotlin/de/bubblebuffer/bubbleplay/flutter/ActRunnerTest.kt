package de.bubblebuffer.bubbleplay.flutter

import de.bubblebuffer.bubbleplay.keys.DeviceKey
import de.bubblebuffer.bubbleplay.log.BpLog
import de.bubblebuffer.bubbleplay.net.PairingListener
import de.bubblebuffer.bubbleplay.auth.PairLink
import de.bubblebuffer.bubbleplay.pcs.PcListStore
import kotlinx.coroutines.runBlocking
import org.json.JSONObject
import org.junit.After
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNotNull
import org.junit.Assert.assertTrue
import org.junit.Assert.fail
import org.junit.Test
import java.io.File
import java.util.concurrent.Executors

/** A failed action reaches the caller (and so Dart); the log carries the cause, never the payload. */
class ActRunnerTest {
    private class MemoryKeys(var failSave: Boolean = false) : KeyStorage {
        var text: String? = null
        override fun load() = text
        override fun save(text: String) {
            if (failSave) throw java.io.IOException("keystore unavailable")
            this.text = text
        }
    }

    private class Holder : AppBackend.UnlockedHolder {
        var key: DeviceKey? = null
        override fun get() = key
        override fun set(key: DeviceKey) {
            this.key = key
        }
    }

    private val pool = Executors.newSingleThreadExecutor()
    private val dir = File.createTempFile("actrunner", "").also { it.delete(); it.mkdirs() }

    @After
    fun tearDown() {
        pool.shutdownNow()
        BpLog.current()?.close()
        BpLog.setCurrent(null)
        dir.deleteRecursively()
    }

    private fun backend(keys: MemoryKeys = MemoryKeys()): AppBackend {
        var pcsText: String? = null
        val pcs = PcListStore(object : PcListStore.Backing {
            override fun read() = pcsText
            override fun write(text: String) {
                pcsText = text
            }
        })
        val pairing = PairingRunner { _: DeviceKey, _: PairLink, _: String, _: PairingListener -> {} }
        return AppBackend(keys, pcs, pairing, "Test phone", "App storage", unlocked = Holder())
    }

    private fun logLines(): List<JSONObject> {
        BpLog.current()!!.flush()
        return dir.walkTopDown().filter { it.isFile && it.name.endsWith(".jsonl") }
            .flatMap { it.readLines() }.map(::JSONObject).toList()
    }

    @Test
    fun aFailingActionThrowsToTheCallerWithoutQuotingThePayload() {
        BpLog.setCurrent(BpLog(dir))
        val runner = ActRunner(backend(), pool)
        val secret = "hunter2-passphrase"
        try {
            runBlocking { runner.act("{\"UnlockImport\":{\"passphrase\":\"$secret\"") } // truncated JSON
            fail("the failure must reach the caller")
        } catch (e: FlutterError) {
            assertEquals(ActRunner.FAILED, e.code)
            assertFalse(e.message.orEmpty().contains(secret))
        }
        val lines = logLines()
        val failure = lines.single { it.getString("msg") == "an app action failed" }
        assertEquals("error", failure.getString("level"))
        assertNotNull(failure.getJSONObject("fields").optString("exception"))
        assertFalse(lines.joinToString("\n").contains(secret))
    }

    @Test
    fun aSucceedingActionReturnsNormally() {
        val runner = ActRunner(backend(), pool)
        runBlocking { runner.act("\"StartPairing\"") }
    }

    @Test
    fun keyCreationFailureLogsItsCauseAndSetsTheDeviceId() {
        BpLog.setCurrent(BpLog(dir))
        val failing = backend(MemoryKeys(failSave = true))
        failing.act("\"CreateKey\"")
        val keys = MemoryKeys()
        backend(keys).act("\"CreateKey\"")
        val lines = logLines()
        val cause = lines.single { it.getString("msg") == "the device key could not be created" }
        assertTrue(cause.getJSONObject("fields").getString("error").contains("keystore unavailable"))
        val created = lines.single { it.getString("msg") == "a device key was created" }
        assertTrue(created.getString("device").isNotEmpty())
    }
}
