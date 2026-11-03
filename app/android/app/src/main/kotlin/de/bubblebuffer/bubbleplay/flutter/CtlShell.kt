package de.bubblebuffer.bubbleplay.flutter

import de.bubblebuffer.bubbleplay.log.CtlPassThrough
import org.json.JSONArray
import org.json.JSONObject

/**
 * The Flutter shell's end of the ctl bridge (docs/architecture/client-log-upload.md): follows the
 * [CtlPassThrough] of the active session, forwards its events to Dart as `(kind, json)` through [emit]
 * (`opened`, `message`, `closed`) and answers the pigeon host calls. The session can be replaced:
 * [attach] on each new one, [attach] with null when it ends.
 *
 * [emit] is called on the session thread; the owner moves it to the main thread. Nothing here throws
 * into the session.
 */
internal class CtlShell(private val emit: (kind: String, json: String) -> Unit) {
    private class Link(val source: CtlPassThrough, val listener: CtlPassThrough.Listener)

    private val lock = Any()
    private var link: Link? = null

    @Volatile private var pcId = ""
    @Volatile private var peerCaps: List<String> = emptyList()
    @Volatile private var announced = false

    /** Follows [source] from now on; the previous one (if any) is let go. */
    fun attach(source: CtlPassThrough?) {
        synchronized(lock) {
            link?.let { old -> old.source.removeListener(old.listener) }
            link = null
            closed()
            if (source == null) return
            val listener = object : CtlPassThrough.Listener {
                override fun onOpened(pcId: String, peerCaps: List<String>) {
                    if (!isCurrent(source)) return
                    this@CtlShell.pcId = pcId
                    this@CtlShell.peerCaps = peerCaps
                    announced = true
                    emit("opened", JSONObject().put("pcId", pcId).put("peerCaps", JSONArray(peerCaps)).toString())
                }

                override fun onMessage(message: JSONObject) {
                    if (isCurrent(source)) emit("message", message.toString())
                }

                override fun onClosed() {
                    if (isCurrent(source)) closed()
                }
            }
            link = Link(source, listener)
            // May call onOpened at once when a connection is already up.
            source.addListener(listener)
        }
    }

    /** Pigeon `ctlSend`: false when there is no session or connection, or the queue refused it. */
    fun send(json: String): Boolean = try {
        current()?.sendRaw(json) ?: false
    } catch (_: Throwable) {
        false
    }

    /** Pigeon `ctlState`. */
    fun state(): CtlState {
        val source = current()
        val open = try {
            source?.isOpen() ?: false
        } catch (_: Throwable) {
            false
        }
        return CtlState(
            open = open,
            bufferedBytes = if (open) source!!.bufferedBytes() else 0L,
            sinceLastInputMs = if (open) source!!.sinceLastInputMs() else Long.MAX_VALUE,
            pcId = if (open) pcId else "",
            peerCaps = if (open) peerCaps else emptyList(),
        )
    }

    private fun current(): CtlPassThrough? = synchronized(lock) { link?.source }

    private fun isCurrent(source: CtlPassThrough) = synchronized(lock) { link?.source === source }

    private fun closed() {
        if (!announced) return
        announced = false
        pcId = ""
        peerCaps = emptyList()
        emit("closed", "{}")
    }
}
