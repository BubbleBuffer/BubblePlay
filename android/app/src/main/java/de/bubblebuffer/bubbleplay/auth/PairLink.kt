package de.bubblebuffer.bubbleplay.auth

import de.bubblebuffer.bubbleplay.keys.DeviceId
import de.bubblebuffer.bubbleplay.keys.DeviceKeyException
import java.io.ByteArrayOutputStream
import java.net.URI
import java.net.URISyntaxException
import java.net.URLEncoder
import java.nio.ByteBuffer
import java.nio.charset.CharacterCodingException
import java.nio.charset.CodingErrorAction

/**
 * A pairing request: the relay origin URL, the canonical PC ID and the normalised 10-character
 * pairing code (lane dk2-android contract; the core run implements the bodies).
 * Link form: bubbleplay://pair?relay=<url>&pc=<id>&code=<code> (decisions item 14).
 *
 * [relay] is an origin (`scheme://host[:port]`, lowercase scheme and host, default port dropped),
 * the form the relay proof and the handshake transcript use.
 */
data class PairLink(val relay: String, val pcId: String, val code: String) {
    fun toUri(): String =
        "bubbleplay://pair?relay=" + encode(relay) + "&pc=" + encode(pcId) + "&code=" + encode(code)

    companion object {
        /** Parses a bubbleplay://pair link; null when it is not a valid pairing link. */
        fun parse(uri: String): PairLink? {
            val parsed = try {
                URI(uri)
            } catch (e: URISyntaxException) {
                return null
            }
            if (!parsed.scheme.equals("bubbleplay", ignoreCase = true)) return null
            if (parsed.isOpaque || parsed.rawAuthority?.equals("pair", ignoreCase = true) != true) return null
            val path = parsed.rawPath ?: ""
            if (path != "" && path != "/") return null
            if (parsed.rawFragment != null) return null
            val params = query(parsed.rawQuery ?: return null) ?: return null
            if (params.keys != setOf("relay", "pc", "code")) return null
            val relay = relayOrigin(params.getValue("relay")) ?: return null
            val pc = try {
                DeviceId.parse(params.getValue("pc"))
            } catch (e: DeviceKeyException) {
                return null
            }
            val code = PairingCode.normalise(params.getValue("code")) ?: return null
            return PairLink(relay, pc, code)
        }

        /**
         * The origin form of an http(s) relay URL, or null when it has user info, a path other
         * than "/", a query or a fragment.
         */
        internal fun relayOrigin(text: String): String? {
            val url = try {
                URI(text)
            } catch (e: URISyntaxException) {
                return null
            }
            val scheme = url.scheme?.lowercase() ?: return null
            if (scheme != "http" && scheme != "https") return null
            if (url.isOpaque || url.rawUserInfo != null) return null
            val host = url.host?.lowercase()?.takeIf { it.isNotEmpty() } ?: return null
            val path = url.rawPath ?: ""
            if (path != "" && path != "/") return null
            if (url.rawQuery != null || url.rawFragment != null) return null
            val port = url.port
            val defaultPort = if (scheme == "https") 443 else 80
            return if (port == -1 || port == defaultPort) "$scheme://$host" else "$scheme://$host:$port"
        }

        /** Splits a raw query; null on a malformed escape, a repeated key or an empty part. */
        private fun query(raw: String): Map<String, String>? {
            val out = LinkedHashMap<String, String>()
            for (part in raw.split('&')) {
                val eq = part.indexOf('=')
                if (eq <= 0) return null
                val key = decode(part.substring(0, eq)) ?: return null
                val value = decode(part.substring(eq + 1)) ?: return null
                if (out.put(key, value) != null) return null
            }
            return out
        }

        /** Percent-decoding with strict UTF-8; '+' stays '+'. */
        private fun decode(text: String): String? {
            val bytes = ByteArrayOutputStream()
            var i = 0
            while (i < text.length) {
                val c = text[i]
                if (c == '%') {
                    if (i + 3 > text.length) return null
                    val value = text.substring(i + 1, i + 3).toIntOrNull(16) ?: return null
                    if (text[i + 1] == '+' || text[i + 1] == '-') return null
                    bytes.write(value)
                    i += 3
                } else {
                    if (c.code > 0x7f) return null
                    bytes.write(c.code)
                    i++
                }
            }
            return try {
                Charsets.UTF_8.newDecoder()
                    .onMalformedInput(CodingErrorAction.REPORT)
                    .onUnmappableCharacter(CodingErrorAction.REPORT)
                    .decode(ByteBuffer.wrap(bytes.toByteArray()))
                    .toString()
            } catch (e: CharacterCodingException) {
                null
            }
        }

        private fun encode(text: String): String = URLEncoder.encode(text, "UTF-8")
    }
}

object PairingCode {
    private const val ALPHABET = "0123456789ABCDEFGHJKMNPQRSTVWXYZ"

    /** Uppercase, O->0, I/L->1, dashes and spaces dropped; null unless 10 Crockford base32 chars. */
    fun normalise(input: String): String? {
        if (input.any { it.code > 0x7f }) return null
        val folded = input.filter { it != '-' && !it.isWhitespace() }.uppercase()
            .replace('O', '0').replace('I', '1').replace('L', '1')
        if (folded.length != 10 || folded.any { it !in ALPHABET }) return null
        return folded
    }

    /** "XXXXX-XXXXX" for a normalised code. */
    fun display(code: String): String {
        require(normalise(code) == code) { "not a normalised pairing code" }
        return code.substring(0, 5) + "-" + code.substring(5)
    }
}
