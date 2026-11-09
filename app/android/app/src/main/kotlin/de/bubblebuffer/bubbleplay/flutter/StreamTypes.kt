package de.bubblebuffer.bubbleplay.flutter

import android.view.Surface
import de.bubblebuffer.bubbleplay.keys.DeviceKey
import de.bubblebuffer.bubbleplay.log.CtlPassThrough
import de.bubblebuffer.bubbleplay.net.Session
import de.bubblebuffer.bubbleplay.pcs.PcEntry

/** What a running session reports to the backend. All calls arrive on the main thread. */
internal interface StreamEvents {
    fun onStatus(status: Session.Status)
    fun onMonitors(monitors: List<Session.Monitor>, error: String?)
    fun onVideoSize(width: Int, height: Int)

    /** A live frame reached the surface (the first picture of this session). */
    fun onLiveFrame()

    /** The PC shows its secure desktop (UAC prompt, lock screen); false once it closes. */
    fun onSecureDesktop(open: Boolean)

    /** Whether the host streams sound (the audio toggle only does something then). */
    fun onSoundCapability(enabled: Boolean)

    /** The session learned when the PC was last seen, or why it cannot be reached (null: no problem). */
    fun onPcState(lastSeen: Long, problem: String?) = Unit

    /** The signaling socket that carried input was lost. */
    fun onInputLost() = Unit

    /** The app stayed in the background past the grace period: end the session like Disconnect. */
    fun onBackgroundExpired() = Unit
}

/** Live numbers for the `Stats` of the snapshot; null where the session does not know yet. */
internal data class StreamStats(
    val width: Int,
    val height: Int,
    val fps: Double?,
    val kbps: Double?,
    val rttMs: Long?,
    val codec: String?,
    val dropped: Long,
)

/** The boundary around one real [Session] (production: [RealStreamPort]); fakes stand in for tests. */
internal interface StreamPort {
    val input: InputSink

    /** The session's ctl pass-through for the log upload; null for a port without a session. */
    val ctl: CtlPassThrough? get() = null
    fun start()
    fun stop()
    fun requestMonitor(selector: String)
    fun setGameMode(game: Boolean)
    fun setSoundMuted(muted: Boolean)
    fun setForeground(value: Boolean)
    fun stats(): StreamStats
    fun attachSurface(surface: Surface)
    fun detachSurfaceBlocking()
}

/** Opens a session for [pc]; [onDeferredInput] asks for a motion flush on the next frame. */
internal fun interface StreamOpener {
    fun open(pc: PcEntry, key: DeviceKey, label: String, events: StreamEvents, onDeferredInput: () -> Unit): StreamPort
}

/** What [AppBackend] drives: the session actions of the app contract. Production: [StreamController]. */
internal interface StreamHost {
    fun open(pc: PcEntry, key: DeviceKey, label: String, events: StreamEvents)
    fun close()
    fun requestMonitor(selector: String)
    fun setGameMode(game: Boolean)
    fun setSoundMuted(muted: Boolean)
    fun stats(): StreamStats?

    /** No stream: the M1 placeholder session, for tests that do not care about video. */
    object None : StreamHost {
        override fun open(pc: PcEntry, key: DeviceKey, label: String, events: StreamEvents) = Unit
        override fun close() = Unit
        override fun requestMonitor(selector: String) = Unit
        override fun setGameMode(game: Boolean) = Unit
        override fun setSoundMuted(muted: Boolean) = Unit
        override fun stats(): StreamStats? = null
    }
}
