package de.bubblebuffer.bubbleplay.flutter

import de.bubblebuffer.bubbleplay.auth.PairLink
import de.bubblebuffer.bubbleplay.keys.DeviceKey
import de.bubblebuffer.bubbleplay.net.PairingListener
import de.bubblebuffer.bubbleplay.net.Session
import de.bubblebuffer.bubbleplay.pcs.PcEntry
import de.bubblebuffer.bubbleplay.pcs.PcListStore
import org.json.JSONObject
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Test
import java.time.ZoneId

/** Session actions of the app contract against a fake session boundary (no network, no Android). */
class AppBackendStreamTest {
    private class FakeHost : StreamHost {
        val log = ArrayList<String>()
        var events: StreamEvents? = null
        var stats: StreamStats? = null
        override fun open(pc: PcEntry, key: DeviceKey, label: String, events: StreamEvents) {
            log += "open ${pc.id} $label"
            this.events = events
        }

        override fun close() {
            log += "close"
        }

        override fun requestMonitor(selector: String) {
            log += "monitor $selector"
        }

        override fun setGameMode(game: Boolean) {
            log += "game $game"
        }

        override fun setSoundMuted(muted: Boolean) {
            log += "muted $muted"
        }

        override fun stats() = stats
    }

    private class Holder : AppBackend.UnlockedHolder {
        var key: DeviceKey? = null
        override fun get() = key
        override fun set(key: DeviceKey) {
            this.key = key
        }
    }

    private class MemoryKeys : KeyStorage {
        var text: String? = null
        override fun load() = text
        override fun save(text: String) {
            this.text = text
        }
    }

    private val noPairing = object : PairingRunner {
        override fun start(key: DeviceKey, link: PairLink, name: String, listener: PairingListener): () -> Unit = {}
    }
    private val pcsText = arrayOf<String?>(null)
    private val pcs = PcListStore(object : PcListStore.Backing {
        override fun read() = pcsText[0]
        override fun write(text: String) {
            pcsText[0] = text
        }
    })
    private val host = FakeHost()
    private var notified = 0
    private val pcId = "77VH-7M56-8GJK-Y12J"
    private val backend = AppBackend(
        MemoryKeys(), pcs, noPairing, "Test phone", "App storage", { 1_000_000_000_000L }, ZoneId.of("UTC"), Holder(), host,
    ).also { it.setNotifier { notified++ } }

    private fun connect(): StreamEvents {
        pcs.put(PcEntry(pcId, "k", "https://relay.example", "Studio", 0))
        backend.act("\"CreateKey\"")
        backend.act("\"CancelFlow\"")
        backend.act("""{"Connect":{"pc_id":"$pcId"}}""")
        return host.events!!
    }

    private fun session() = JSONObject(backend.snapshot()).getJSONObject("session")

    private fun noSession() = JSONObject(backend.snapshot()).isNull("session")

    private fun monitors(vararg picked: Boolean) =
        picked.mapIndexed { i, p -> Session.Monitor("DISPLAY${i + 1}", "Display ${i + 1}", 1920, 1080, p) }

    @Test
    fun connectOpensTheSessionWithTheDeviceLabelAndDisconnectClosesIt() {
        connect()
        assertEquals(listOf("close", "open $pcId Test phone"), host.log)
        assertEquals("Studio", session().getString("pc_name"))
        backend.act("\"Disconnect\"")
        assertEquals("close", host.log.last())
        assertTrue(noSession())
    }

    @Test
    fun backgroundGraceExpiryDisconnectsLikeDisconnectAndTheNextConnectIsNormal() {
        val events = connect()
        events.onBackgroundExpired()
        assertEquals("close", host.log.last())
        assertTrue(noSession())
        host.log.clear()
        backend.act("""{"Connect":{"pc_id":"$pcId"}}""")
        assertEquals(listOf("close", "open $pcId Test phone"), host.log)
        assertFalse(noSession())
        // The expired session's late events do nothing to the new one.
        events.onBackgroundExpired()
        assertFalse(noSession())
    }

    @Test
    fun modeAndAudioGoToTheSession() {
        connect()
        host.log.clear()
        backend.act("""{"SetMode":"Game"}""")
        backend.act("""{"SetAudio":false}""")
        assertEquals(listOf("game true", "muted true"), host.log)
        assertEquals("Game", session().getString("mode"))
        assertFalse(session().getBoolean("audio"))
    }

    @Test
    fun monitorListShowsInTheSnapshotAndAPickAsksTheHost() {
        val events = connect()
        events.onMonitors(monitors(true, false), null)
        val s = session()
        assertEquals(2, s.getJSONArray("monitors").length())
        assertEquals("DISPLAY2", s.getJSONArray("monitors").getJSONObject(1).getString("selector"))
        assertEquals(1920, s.getJSONArray("monitors").getJSONObject(1).getInt("x"))
        assertEquals(0, s.getInt("monitor"))

        backend.act("""{"SetMonitor":1}""")
        assertEquals("monitor DISPLAY2", host.log.last())
        assertTrue(session().getBoolean("monitor_pending"))
        assertEquals(1, session().getInt("monitor"))

        // A stale list that still shows the old screen keeps the pick pending.
        events.onMonitors(monitors(true, false), null)
        assertTrue(session().getBoolean("monitor_pending"))
        events.onMonitors(monitors(false, true), null)
        assertFalse(session().getBoolean("monitor_pending"))
        assertEquals(1, session().getInt("monitor"))
    }

    @Test
    fun monitorPickOutsideTheListIsIgnored() {
        val events = connect()
        events.onMonitors(monitors(true), null)
        backend.act("""{"SetMonitor":3}""")
        assertFalse(host.log.any { it.startsWith("monitor") })
    }

    @Test
    fun monitorErrorShowsAndClearsThePending() {
        val events = connect()
        events.onMonitors(monitors(true, false), null)
        backend.act("""{"SetMonitor":1}""")
        events.onMonitors(emptyList(), "The PC could not switch displays.")
        assertEquals("The PC could not switch displays.", session().getString("monitor_error"))
        assertFalse(session().getBoolean("monitor_pending"))
    }

    @Test
    fun reconnectingAndLiveFollowTheSessionStatus() {
        val events = connect()
        assertEquals("Live", session().getString("link"))
        events.onStatus(Session.Status(Session.State.RECONNECTING, "Reconnecting"))
        val link = session().getJSONObject("link").getJSONObject("Reconnecting")
        assertEquals(1, link.getInt("attempt"))
        events.onStatus(Session.Status(Session.State.CONNECTED, "Connected"))
        assertEquals("Live", session().getString("link"))
    }

    @Test
    fun terminalErrorEndsTheSessionAndStoresTheProblemOnThePc() {
        val events = connect()
        events.onPcState(5L, "problem_offline")
        events.onStatus(Session.Status(Session.State.ERROR, "PC offline", problem = "problem_offline"))
        assertTrue(noSession())
        assertEquals("close", host.log.last())
        assertEquals("problem_offline", pcs.get(pcId)!!.problem)
    }

    @Test
    fun secureDesktopAndSoundCapabilityShowAsNoticeAndControl() {
        val events = connect()
        events.onSoundCapability(true)
        assertTrue(session().getBoolean("audio_controls_available"))
        events.onSecureDesktop(true)
        assertEquals("SecureDesktop", session().getString("notice"))
        events.onSecureDesktop(false)
        assertTrue(session().isNull("notice"))
    }

    @Test
    fun statsComeFromTheSessionAndNotifyOnlyWhenTheyChange() {
        connect()
        host.stats = StreamStats(1920, 1080, 59.5, 12_000.0, 18, "HEVC", 3)
        val before = notified
        backend.refreshStats()
        assertEquals(before + 1, notified)
        backend.refreshStats()
        assertEquals(before + 1, notified)
        val s = session()
        val stats = s.getJSONObject("stats")
        assertEquals("1920x1080", stats.getString("resolution"))
        assertEquals(59.5, stats.getDouble("fps"), 0.01)
        assertEquals(12.0, stats.getDouble("bitrate_mbps"), 0.01)
        assertEquals(18, stats.getInt("rtt_ms"))
        assertEquals(18, s.getInt("rtt_ms"))
        assertEquals(3, stats.getInt("dropped"))
        assertEquals("HEVC", stats.getString("codec"))
    }

    @Test
    fun eventsOfAnEndedSessionAreIgnored() {
        val old = connect()
        backend.act("\"Disconnect\"")
        val before = notified
        old.onMonitors(monitors(true), null)
        old.onStatus(Session.Status(Session.State.RECONNECTING, "x"))
        assertEquals(before, notified)
        assertTrue(noSession())
    }

    @Test
    fun aSecondConnectClosesTheFirstAndOldEventsDoNotLeakIn() {
        val first = connect()
        backend.act("""{"Connect":{"pc_id":"$pcId"}}""")
        assertEquals(listOf("close", "open", "close", "open"), host.log.map { it.split(" ")[0] })
        first.onMonitors(monitors(true, true), null)
        assertEquals(0, session().getJSONArray("monitors").length())
    }
}
