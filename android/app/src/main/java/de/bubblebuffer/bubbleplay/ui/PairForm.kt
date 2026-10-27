package de.bubblebuffer.bubbleplay.ui

import de.bubblebuffer.bubbleplay.keys.DeviceId
import de.bubblebuffer.bubbleplay.keys.DeviceKeyException
import de.bubblebuffer.bubbleplay.pcs.PcEntry
import java.net.URI
import java.net.URISyntaxException

/** Pure checks behind the pairing form: the relay origin, the PC ID, names, and the relay to suggest. */
internal object PairForm {
    /**
     * The origin (scheme://host[:port]) of a typed relay address, or null. Only http:// and https:// with
     * a host are accepted; a path, query or fragment is dropped, and scheme and host are lowercased.
     */
    fun relayOrigin(input: String): String? {
        val uri = try {
            URI(input.trim())
        } catch (e: URISyntaxException) {
            return null
        }
        val scheme = uri.scheme?.lowercase() ?: return null
        if (scheme != "https" && scheme != "http") return null
        val host = uri.host?.lowercase() ?: return null
        if (uri.rawUserInfo != null) return null
        val port = if (uri.port == -1) "" else ":${uri.port}"
        return "$scheme://$host$port"
    }

    /** The canonical PC ID for typed input (any case, dashes or spaces), or null. */
    fun pcId(input: String): String? = try {
        DeviceId.parse(input.filterNot { it.isWhitespace() })
    } catch (e: DeviceKeyException) {
        null
    }

    /** The name for a PC that has none: "PC " and the first four characters of its ID. */
    fun defaultName(pcId: String): String = "PC " + pcId.take(4)

    /** The typed name, trimmed; blank means the default for [pcId]. */
    fun name(input: String, pcId: String): String = input.trim().ifEmpty { defaultName(pcId) }

    /** The name a PC shows in the list: its own, or the default when it has none. */
    fun displayName(pc: PcEntry): String = pc.name.ifEmpty { defaultName(pc.id) }

    /** The relay to suggest on a new pairing: the one of the most recently paired PC, or null. */
    fun suggestedRelay(pcs: List<PcEntry>): String? = pcs.maxByOrNull { it.paired }?.relay
}
