package de.bubblebuffer.bubbleplay.net

import de.bubblebuffer.bubbleplay.auth.PairLink
import de.bubblebuffer.bubbleplay.keys.DeviceKey
import de.bubblebuffer.bubbleplay.pcs.PcEntry

/** Progress of one pairing attempt, for the pairing screen. */
enum class PairingStage {
    /** Opening /ws and proving this device's key to the relay. */
    Connecting,
    /** Admitted in pairing mode; exchanging keys and pairing proofs with the PC. */
    Proving,
    /** Proofs verified; the comparison number is shown and the PC owner decides. */
    WaitingForPc,
}

enum class PairingFailure {
    /** The relay closed before admission (1008): wrong or expired code, no pairing window, or rate limit. */
    Refused,
    /** The PC's pairing proof did not verify against the code. */
    ProofMismatch,
    /** The PC's ID, key or handshake signature is invalid. */
    PcKeyInvalid,
    /** The socket dropped after admission. */
    ConnectionLost,
    /** No answer from the relay or the PC in time. */
    Timeout,
}

/** Callbacks run on the main thread. Exactly one of onPaired, onDenied, onFailed ends an attempt. */
interface PairingListener {
    fun onStage(stage: PairingStage)
    /** The comparison number, display form "482 913". */
    fun onNumber(display: String)
    /** The PC sent pair-done. [pc] carries the pinned key; the caller stores it. */
    fun onPaired(pc: PcEntry)
    fun onDenied()
    fun onFailed(reason: PairingFailure)
}

/**
 * Runs one pairing at the PC (lane dk2-android contract; the session run implements it).
 * [name] becomes [PcEntry.name] of the result.
 */
class PairingClient(
    private val key: DeviceKey,
    private val link: PairLink,
    private val name: String,
    private val listener: PairingListener,
) {
    fun start(): Unit = throw NotImplementedError("PairingClient: session run")

    /** Ends the attempt; no listener call follows. */
    fun cancel(): Unit = throw NotImplementedError("PairingClient: session run")
}
