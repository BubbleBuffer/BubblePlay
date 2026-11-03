package de.bubblebuffer.bubbleplay.flutter

import android.view.Surface
import de.bubblebuffer.bubbleplay.input.RemoteInput
import de.bubblebuffer.bubbleplay.keys.DeviceKey
import de.bubblebuffer.bubbleplay.net.ServerConfig
import de.bubblebuffer.bubbleplay.net.Session
import de.bubblebuffer.bubbleplay.pcs.PcEntry
import de.bubblebuffer.bubbleplay.protocol.CursorKind
import de.bubblebuffer.bubbleplay.protocol.CursorState
import de.bubblebuffer.bubbleplay.protocol.FileWire
import de.bubblebuffer.bubbleplay.net.SecureDesktop
import de.bubblebuffer.bubbleplay.net.ViewerGrant

/** The real [Session] of the native app, built the way `StreamActivity.startSession` builds it. */
internal object RealStreamOpener : StreamOpener {
    override fun open(
        pc: PcEntry,
        key: DeviceKey,
        label: String,
        events: StreamEvents,
        onDeferredInput: () -> Unit,
    ): StreamPort {
        val relay = ServerConfig.parseServer(pc.relay) ?: throw IllegalArgumentException("bad relay address")
        val config = ServerConfig(relay, pc.id)
        val adapter = Adapter(events)
        // Same construction as StreamActivity: the codec follows the hardware (the native
        // "automatic" preference), the label is the device name.
        val session = Session(
            config, pc, key, label.filterNot { Character.isISOControl(it) }.take(64), Session.chooseCodec("auto"), adapter,
            events::onPcState,
        )
        val sink = RemoteInputSink(RemoteInput(session, {}, onDeferredInput))
        adapter.bind(session, sink)
        return RealStreamPort(session, sink)
    }

    /** Session callbacks to [StreamEvents]; what a Flutter texture has no use for is dropped. */
    private class Adapter(private val events: StreamEvents) : Session.Listener {
        private var session: Session? = null
        private var sink: InputSink? = null

        fun bind(session: Session, sink: InputSink) {
            this.session = session
            this.sink = sink
        }

        override fun onStatus(status: Session.Status) = events.onStatus(status)
        override fun onMonitors(monitors: List<Session.Monitor>, error: String?) = events.onMonitors(monitors, error)
        override fun onVideoSize(width: Int, height: Int) {
            events.onVideoSize(width, height)
            // A screen switch usually changes the frame: refresh which monitor is current.
            session?.requestMonitorList()
        }

        override fun onLiveFrame() = events.onLiveFrame()
        override fun onSecureDesktop(state: SecureDesktop?) = events.onSecureDesktop(state != null)
        override fun onSoundCapability(enabled: Boolean) = events.onSoundCapability(enabled)

        override fun onInputReady() {
            sink?.inputReady()
            session?.requestMonitorList()
        }

        override fun onInputLost() {
            // The host drops held input with its session; mirror that so nothing stays down.
            sink?.inputLost()
            events.onInputLost()
        }

        // No drawn cursor, clipboard, files, follow, IME or secure-prompt answer in this milestone.
        override fun onCursor(state: CursorState) = Unit
        override fun onCursorType(kind: CursorKind) = Unit
        override fun onRemoteClipboard(text: String) = Unit
        override fun onSecureActionResult(ok: Boolean, reason: String) = Unit
        override fun onDesktopChanged() = Unit
        override fun onFileCapability(enabled: Boolean?) = Unit
        override fun onFileEpoch(epoch: String?) = Unit
        override fun onFileOffer(offer: FileWire.Offer) = Unit
        override fun onFollowCapability(enabled: Boolean) = Unit
        override fun onFollowState(follow: Boolean) = Unit
        override fun onViewerGrant(grant: ViewerGrant) = Unit
    }
}

internal class RealStreamPort(
    private val session: Session,
    override val input: InputSink,
) : StreamPort {
    override val ctl get() = session.ctl

    override fun start() {
        session.start()
    }

    override fun stop() = session.stop()
    override fun requestMonitor(selector: String) {
        session.requestMonitor(selector)
    }

    override fun setGameMode(game: Boolean) {
        session.setStreamMode(game)
    }

    override fun setSoundMuted(muted: Boolean) = session.setSoundMuted(muted)

    override fun setForeground(value: Boolean) {
        session.setForeground(value)
        session.setSoundForeground(value)
    }

    override fun stats(): StreamStats {
        val d = session.diagnostics
        return StreamStats(d.width, d.height, d.fps, d.kbps, d.rttMs, d.codec, session.decoder.skippedFrames)
    }

    override fun attachSurface(surface: Surface) {
        session.decoder.setSurface(surface)
    }

    override fun detachSurfaceBlocking() = session.decoder.detachSurfaceBlocking()
}
