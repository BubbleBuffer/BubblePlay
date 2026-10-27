package de.bubblebuffer.bubbleplay.pcs

import android.content.Context

/**
 * A paired PC as the viewer stores it. Everything here is public: no secret is kept per PC.
 *
 * [id] is the canonical PC ID (XXXX-XXXX-XXXX-XXXX), [publicKey] the PC's Ed25519 public key
 * (base64url, 32 bytes) pinned at pairing, [relay] the relay's origin URL (https://host[:port]),
 * [name] the user's label for the PC, [paired] the pairing time in epoch milliseconds.
 * [lastSeen] is the last successful connection in epoch milliseconds (0 when unknown), and
 * [problem] is the latest connection problem, cleared by a successful connection.
 */
data class PcEntry(
    val id: String,
    val publicKey: String,
    val relay: String,
    val name: String,
    val paired: Long,
    val lastSeen: Long = 0,
    val problem: String? = null,
)

/**
 * The viewer's PC list (lane dk2-android contract; the UI run implements it). Backed by the
 * app's SharedPreferences as JSON. All calls are cheap and may run on the main thread.
 */
interface PcStore {
    fun list(): List<PcEntry>
    fun get(id: String): PcEntry?
    /** Adds or replaces the entry with the same [PcEntry.id]. */
    fun put(entry: PcEntry)
    fun rename(id: String, name: String)
    /** Updates connection metadata for an existing PC; null [problem] clears an old problem. */
    fun touch(id: String, lastSeen: Long, problem: String?)
    fun remove(id: String)

    companion object {
        fun from(context: Context): PcStore = PcListStore.preferences(context)
    }
}
