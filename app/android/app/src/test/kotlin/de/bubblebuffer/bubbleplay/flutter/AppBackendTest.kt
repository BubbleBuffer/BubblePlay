package de.bubblebuffer.bubbleplay.flutter

import de.bubblebuffer.bubbleplay.auth.PairLink
import de.bubblebuffer.bubbleplay.keys.DeviceKey
import de.bubblebuffer.bubbleplay.keys.KeyFileCodec
import de.bubblebuffer.bubbleplay.net.PairingFailure
import de.bubblebuffer.bubbleplay.net.PairingListener
import de.bubblebuffer.bubbleplay.pcs.PcEntry
import de.bubblebuffer.bubbleplay.pcs.PcListStore
import org.json.JSONObject
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNotNull
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Test
import java.time.ZoneId

/** The Android backend against fakes: the same behaviour the Rust backend tests pin on Windows. */
class AppBackendTest {
    private class MemoryKeys : KeyStorage {
        var text: String? = null
        override fun load() = text
        override fun save(text: String) {
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

    private class FakePairing : PairingRunner {
        var link: PairLink? = null
        var listener: PairingListener? = null
        var cancelled = false
        override fun start(key: DeviceKey, link: PairLink, name: String, listener: PairingListener): () -> Unit {
            this.link = link
            this.listener = listener
            return { cancelled = true }
        }
    }

    private class Fixture(val keys: MemoryKeys = MemoryKeys(), val pcsText: Array<String?> = arrayOf(null)) {
        val holder = Holder()
        val pairing = FakePairing()
        val pcs = PcListStore(object : PcListStore.Backing {
            override fun read() = pcsText[0]
            override fun write(text: String) {
                pcsText[0] = text
            }
        })
        var now = 1_000_000_000_000L
        var notified = 0

        fun backend() = AppBackend(
            keys, pcs, pairing, "Test phone", "App storage", { now }, ZoneId.of("UTC"), holder,
        ).also { it.setNotifier { notified++ } }
    }

    private fun AppBackend.state() = JSONObject(snapshot())

    private fun confirm(backend: AppBackend) = backend.state().getJSONObject("flow").getJSONObject("PairConfirm")

    private fun importAction(name: String, text: String) =
        JSONObject().put("ImportKeyText", JSONObject().put("file_name", name).put("text", text)).toString()

    @Test
    fun freshInstallAsksToCreateOrImportAKey() {
        val state = Fixture().backend().state()
        assertEquals("", state.getJSONObject("this_device").getString("id"))
        assertEquals("Choose", state.getJSONObject("flow").getString("Enroll"))
        assertEquals(0, state.getJSONArray("pcs").length())
        assertTrue(state.isNull("host"))
    }

    @Test
    fun createKeyShowsTheIdAndASecondCreateNeverReplacesIt() {
        val f = Fixture()
        val backend = f.backend()
        backend.act("\"CreateKey\"")
        val state = backend.state()
        assertEquals("Created", state.getJSONObject("flow").getString("Enroll"))
        val id = state.getJSONObject("this_device").getString("id")
        assertEquals(19, id.length)
        assertNotNull(f.holder.key)

        backend.act("\"CancelFlow\"")
        assertTrue(backend.state().isNull("flow"))
        backend.act("\"CreateKey\"")
        assertEquals(id, backend.state().getJSONObject("this_device").getString("id"))

        // A restart finds the stored key and needs no flow.
        val reopened = Fixture(f.keys, f.pcsText).backend().state()
        assertTrue(reopened.isNull("flow"))
        assertEquals(id, reopened.getJSONObject("this_device").getString("id"))
        assertEquals(id, KeyFileCodec.parse(backend.exportKey()).id)
    }

    @Test
    fun aFileThatIsNotAKeyIsRefusedAndCancelReturnsToTheChoice() {
        val backend = Fixture().backend()
        backend.act(importAction("notes.txt", "hello"))
        val refused = backend.state().getJSONObject("flow").getJSONObject("Enroll").getJSONObject("ImportRefused")
        assertEquals("notes.txt", refused.getString("file_name"))
        assertTrue(refused.getString("reason").isNotEmpty())
        backend.act("\"CancelFlow\"")
        assertEquals("Choose", backend.state().getJSONObject("flow").getString("Enroll"))
    }

    @Test
    fun aProtectedImportAsksForItsPassphraseOnceAndIsStoredWithoutIt() {
        val f = Fixture()
        val backend = f.backend()
        val created = KeyFileCodec.create("Other", "correct horse")
        backend.act(importAction("protected.bpkey", KeyFileCodec.serialize(created.file)))
        fun unlock() = backend.state().getJSONObject("flow").getJSONObject("Enroll").getJSONObject("ImportUnlock")
        assertTrue(unlock().isNull("error"))

        backend.act("""{"UnlockImport":{"passphrase":"wrong"}}""")
        assertFalse(unlock().isNull("error"))
        assertEquals("", backend.state().getJSONObject("this_device").getString("id"))

        backend.act("""{"UnlockImport":{"passphrase":"correct horse"}}""")
        val state = backend.state()
        assertTrue(state.isNull("flow"))
        assertEquals(created.file.id, state.getJSONObject("this_device").getString("id"))
        assertFalse(KeyFileCodec.parse(f.keys.text!!).isProtected)
        created.key.close()
    }

    @Test
    fun pairingInputIsCheckedBeforeAnythingIsSent() {
        val f = Fixture()
        val backend = f.backend()
        backend.act("\"CreateKey\"")
        backend.act("\"CancelFlow\"")
        backend.act("\"StartPairing\"")
        fun submit(pc: String, code: String, relay: String): String {
            val args = JSONObject().put("link", JSONObject.NULL).put("pc_id", pc).put("code", code).put("relay", relay)
            backend.act(JSONObject().put("SubmitPairing", args).toString())
            return backend.state().getJSONObject("flow").getJSONObject("PairEnter").getString("error")
        }
        assertTrue(submit("nope", "70Q1K-9MXR1", "https://relay.example").contains("PC ID"))
        assertTrue(submit("77VH-7M56-8GJK-Y12J", "xx", "https://relay.example").contains("pairing code"))
        assertTrue(submit("77VH-7M56-8GJK-Y12J", "70Q1K-9MXR1", " ").contains("relay"))
        assertNull(f.pairing.link)
    }

    @Test
    fun pairingLinkRunsThePairingAndStoresThePcWhenApproved() {
        val f = Fixture()
        val backend = f.backend()
        backend.act("\"CreateKey\"")
        backend.act("\"CancelFlow\"")
        val link = "bubbleplay://pair?relay=https%3A%2F%2Frelay.example&pc=77VH-7M56-8GJK-Y12J&code=70Q1K-9MXR1"
        backend.act("""{"SubmitPairing":{"link":"$link","pc_id":"","code":"","relay":""}}""")
        assertEquals("77VH-7M56-8GJK-Y12J", f.pairing.link!!.pcId)
        assertEquals("https://relay.example", f.pairing.link!!.relay)
        assertEquals("Waiting", confirm(backend).getString("outcome"))

        f.now += 5_000
        f.pairing.listener!!.onNumber("482 913")
        assertEquals(482913, confirm(backend).getInt("number"))
        assertEquals(5, confirm(backend).getInt("waiting_secs"))

        f.pairing.listener!!.onPaired(PcEntry("77VH-7M56-8GJK-Y12J", "k", "https://relay.example", "Studio", f.now))
        assertEquals("Pc", confirm(backend).getJSONObject("outcome").getJSONObject("Approved").getString("by"))
        assertEquals("Studio", backend.state().getJSONArray("pcs").getJSONObject(0).getString("name"))
    }

    @Test
    fun anUnreachableRelayReturnsToTheFormAndCancelDropsALateResult() {
        val f = Fixture()
        val backend = f.backend()
        backend.act("\"CreateKey\"")
        backend.act("\"CancelFlow\"")
        val submit = """{"SubmitPairing":{"link":null,"pc_id":"77VH-7M56-8GJK-Y12J","code":"70Q1K-9MXR1","relay":"https://relay.example"}}"""
        backend.act(submit)
        f.pairing.listener!!.onFailed(PairingFailure.ConnectionLost)
        val error = backend.state().getJSONObject("flow").getJSONObject("PairEnter").getString("error")
        assertTrue(error, error.contains("relay"))

        backend.act(submit)
        val late = f.pairing.listener!!
        backend.act("\"CancelFlow\"")
        assertTrue(f.pairing.cancelled)
        late.onPaired(PcEntry("77VH-7M56-8GJK-Y12J", "k", "https://relay.example", "x", 0))
        assertTrue(backend.state().isNull("flow"))
    }

    @Test
    fun connectOpensThePlaceholderSessionAndDisconnectClosesIt() {
        val f = Fixture()
        f.pcs.put(PcEntry("77VH-7M56-8GJK-Y12J", "k", "https://relay.example", "", 0, f.now - 60_000))
        val backend = f.backend()
        backend.act("\"CreateKey\"")
        backend.act("\"CancelFlow\"")
        val pc = backend.state().getJSONArray("pcs").getJSONObject(0)
        assertEquals("PC 77VH", pc.getString("name"))
        assertTrue(pc.getString("last_seen").startsWith("Today "))

        backend.act("""{"Rename":{"pc_id":"77VH-7M56-8GJK-Y12J","name":"Studio"}}""")
        backend.act("""{"Connect":{"pc_id":"77VH-7M56-8GJK-Y12J"}}""")
        assertEquals("Studio", backend.state().getJSONObject("session").getString("pc_name"))
        backend.act("""{"SetMode":"Game"}""")
        assertEquals("Game", backend.state().getJSONObject("session").getString("mode"))
        backend.act("\"Disconnect\"")
        assertTrue(backend.state().isNull("session"))
        backend.act("""{"Forget":{"pc_id":"77VH-7M56-8GJK-Y12J"}}""")
        assertEquals(0, backend.state().getJSONArray("pcs").length())
        assertTrue(f.notified > 0)
    }
}
