package de.bubblebuffer.bubbleplay.auth

import java.security.MessageDigest

/** What [ViewerAuth] reports. */
sealed class AuthEvent {
    /** Normal mode done. Route every later endpoint message through [channel] (decisions item 15). */
    class Authenticated(val channel: CtlChannel, val pcId: String, pcPublic: ByteArray) : AuthEvent() {
        private val key = pcPublic.copyOf()
        val pcPublic: ByteArray get() = key.copyOf()
    }

    /** Pairing: both macs verified; show [display] (e.g. "830 687") and "Confirm on the PC". */
    data class PairNumber(val number: String, val display: String) : AuthEvent()

    /** Pairing: the PC allowed this viewer. Store the PC; then reconnect normally. */
    class Paired(val pcId: String, pcPublic: ByteArray) : AuthEvent() {
        private val key = pcPublic.copyOf()
        val pcPublic: ByteArray get() = key.copyOf()
    }

    /** Pairing: the PC's user denied. */
    object Denied : AuthEvent()

    /** The session must end; [reason] picks the user-visible message. */
    data class Failed(val reason: AuthFailure) : AuthEvent()
}

/** One step of [ViewerAuth]: messages to send, in order, then the event (if any). */
data class AuthStep(val send: List<String>, val event: AuthEvent? = null)

/**
 * The viewer's auth state machine for one session (decisions items 16 and 17). Pure: the runner
 * sends [AuthStep.send] in order and acts on [AuthStep.event].
 *
 * Normal: [start] sends auth-hello; auth-host -> send auth-finish -> [AuthEvent.Authenticated].
 * The PC's first ctl message is then kind "caps" ([CtlChannel.KIND_CAPS]); handle it on the channel.
 *
 * Pairing: [start] sends auth-hello with pair-v1; auth-host -> send auth-finish and pair-proof
 * (viewer mac); the PC's pair-proof is checked in constant time (wrong -> Failed, no number) ->
 * [AuthEvent.PairNumber]; then pair-done -> [AuthEvent.Paired] or pair-denied -> [AuthEvent.Denied].
 * pair-denied is also accepted while waiting for the PC's pair-proof.
 *
 * Feed only auth-host, pair-proof, pair-done and pair-denied (and in normal mode nothing after
 * Authenticated: ctl goes to the channel). Any other type, a repeat, or a message out of order
 * gives [AuthEvent.Failed]; after a terminal event every further message gives Failed
 * UNEXPECTED_MESSAGE. The runner enforces [AUTH_HOST_TIMEOUT_MS] between [start] and auth-host.
 */
class ViewerAuth(private val handshake: Handshake) {
    private enum class State { NEW, AWAIT_AUTH_HOST, AWAIT_PC_PROOF, AWAIT_DECISION, DONE }

    private var state = State.NEW
    private var result: HandshakeResult? = null
    private var pairKey: ByteArray? = null

    val isPairing: Boolean get() = handshake.isPairing

    /** The first step: send auth-hello. */
    @Synchronized
    fun start(): AuthStep {
        if (state != State.NEW) return fail(AuthFailure.UNEXPECTED_MESSAGE)
        state = State.AWAIT_AUTH_HOST
        return AuthStep(listOf(handshake.authHello))
    }

    /** One incoming signalling message (JSON text). */
    @Synchronized
    fun onMessage(text: String): AuthStep {
        val m = try {
            AuthJson.parseObject(text)
        } catch (e: AuthJson.Malformed) {
            return fail(AuthFailure.MALFORMED)
        }
        val type = m["type"] as? String ?: return fail(AuthFailure.MALFORMED)
        return when {
            type == "auth-host" && state == State.AWAIT_AUTH_HOST -> onAuthHost(text)
            type == "pair-proof" && state == State.AWAIT_PC_PROOF -> onPcProof(m)
            type == "pair-done" && state == State.AWAIT_DECISION -> {
                if (m.size != 1) return fail(AuthFailure.MALFORMED)
                val done = result!!
                finish(AuthStep(emptyList(), AuthEvent.Paired(done.pcId, done.pcPublic)))
            }
            type == "pair-denied" && (state == State.AWAIT_PC_PROOF || state == State.AWAIT_DECISION) -> {
                if (m.size != 1) return fail(AuthFailure.MALFORMED)
                finish(AuthStep(emptyList(), AuthEvent.Denied))
            }
            else -> fail(AuthFailure.UNEXPECTED_MESSAGE)
        }
    }

    private fun onAuthHost(text: String): AuthStep {
        val done = try {
            handshake.onAuthHost(text)
        } catch (e: AuthException) {
            return fail(e.failure)
        }
        result = done
        if (!handshake.isPairing) {
            return finish(AuthStep(listOf(done.authFinish), AuthEvent.Authenticated(done.controlKeys.viewerChannel(), done.pcId, done.pcPublic)))
                .also { done.controlKeys.wipe() }
        }
        done.controlKeys.wipe()
        val code = (handshake.mode as Handshake.Mode.Pairing).code
        val k = Pairing.pairKey(code)
        pairKey = k
        val mac = Pairing.viewerMac(k, done.transcriptHash)
        val proof = AuthJson.write(linkedMapOf("type" to "pair-proof", "mac" to AuthCrypto.b64(mac)))
        state = State.AWAIT_PC_PROOF
        return AuthStep(listOf(done.authFinish, proof))
    }

    private fun onPcProof(m: Map<String, Any?>): AuthStep {
        val mac = try {
            AuthJson.requireExactKeys(m, "type", "mac")
            AuthCrypto.unb64(AuthJson.string(m, "mac"), 32)
        } catch (e: AuthJson.Malformed) {
            return fail(AuthFailure.MALFORMED)
        }
        val done = result!!
        val hash = done.transcriptHash
        val expected = Pairing.pcMac(pairKey!!, hash)
        if (!MessageDigest.isEqual(expected, mac)) return fail(AuthFailure.PAIR_MAC_MISMATCH)
        wipePairKey()
        val number = Pairing.sasNumber(hash)
        state = State.AWAIT_DECISION
        return AuthStep(emptyList(), AuthEvent.PairNumber(number, Pairing.sasDisplay(number)))
    }

    private fun finish(step: AuthStep): AuthStep {
        state = State.DONE
        wipePairKey()
        return step
    }

    private fun fail(reason: AuthFailure): AuthStep = finish(AuthStep(emptyList(), AuthEvent.Failed(reason)))

    private fun wipePairKey() {
        pairKey?.fill(0)
        pairKey = null
    }

    companion object {
        /** The viewer waits at most this long for auth-host after auth-hello (decisions item 16). */
        const val AUTH_HOST_TIMEOUT_MS = Handshake.AUTH_HOST_TIMEOUT_MS
    }
}
