package de.bubblebuffer.bubbleplay.flutter

import org.json.JSONArray
import org.json.JSONObject

/**
 * Builds the JSON of the app contract (`AppState`, serde's externally tagged shape) that the Rust
 * model produces on Windows. `app/test/fixtures/contract/states.json` is the shared reference;
 * `StateJsonTest` checks these builders against it.
 */
internal object StateJson {
    fun obj(vararg fields: Pair<String, Any?>): JSONObject {
        val json = JSONObject()
        for ((key, value) in fields) json.put(key, value ?: JSONObject.NULL)
        return json
    }

    private fun tagged(tag: String, payload: Any?): JSONObject = JSONObject().put(tag, payload ?: JSONObject.NULL)

    fun enroll(step: Any): JSONObject = tagged("Enroll", step)
    val choose: JSONObject get() = enroll("Choose")
    val created: JSONObject get() = enroll("Created")

    fun importUnlock(fileName: String, error: String?): JSONObject =
        enroll(tagged("ImportUnlock", obj("file_name" to fileName, "error" to error)))

    fun importRefused(fileName: String, reason: String): JSONObject =
        enroll(tagged("ImportRefused", obj("file_name" to fileName, "reason" to reason)))

    fun pairEnter(remote: Boolean, error: String?): JSONObject =
        tagged("PairEnter", obj("remote" to remote, "error" to error))

    fun pairConfirm(
        pcName: String,
        pcId: String,
        number: Int,
        waitingSecs: Int,
        remote: Boolean,
        outcome: Any,
    ): JSONObject = tagged(
        "PairConfirm",
        obj(
            "pc_name" to pcName,
            "pc_id" to pcId,
            "number" to number,
            "waiting_secs" to waitingSecs,
            "remote" to remote,
            "outcome" to outcome,
        ),
    )

    fun approvedByPc(): JSONObject = tagged("Approved", obj("by" to "Pc"))
    fun deniedByPc(): JSONObject = tagged("Denied", obj("by" to "Pc"))

    fun pc(id: String, name: String, relay: String, lastSeen: String?): JSONObject = obj(
        "id" to id,
        "name" to name,
        "kind" to "Desktop",
        "relay" to relay,
        // Presence is not asked yet: false means "not known to be online".
        "online" to false,
        "last_mode" to null,
        "last_seen" to lastSeen,
        "allowed" to "View and input",
        "vouch_request" to null,
        "problem" to null,
        "default_monitor" to null,
    )

    fun session(pcId: String, pcName: String, mode: String, quality: String, audio: Boolean): JSONObject = obj(
        "pc_id" to pcId,
        "pc_name" to pcName,
        "mode" to mode,
        "rtt_ms" to 0,
        "monitors" to JSONArray(),
        "monitor" to 0,
        "monitor_pending" to false,
        "monitor_error" to null,
        "quality" to quality,
        "quality_controls_available" to false,
        "audio" to audio,
        "audio_controls_available" to false,
        "captured" to false,
        "link" to "Live",
        "notice" to null,
        "stats" to obj(
            "resolution" to "",
            "fps" to 0.0,
            "bitrate_mbps" to 0.0,
            "rtt_ms" to 0,
            "decode_ms" to 0.0,
            "dropped" to 0,
            "transport" to "",
            "codec" to "",
            "frame_ms" to JSONArray(),
        ),
    )

    fun state(
        id: String,
        keyPath: String,
        pcs: List<JSONObject>,
        flow: JSONObject?,
        session: JSONObject?,
    ): JSONObject = obj(
        "this_device" to obj("id" to id, "key_path" to keyPath),
        "pcs" to JSONArray(pcs),
        "host" to null,
        "flow" to flow,
        "session" to session,
    )
}
