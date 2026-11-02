package de.bubblebuffer.bubbleplay.pcs

import de.bubblebuffer.bubbleplay.log.BpLog
import org.json.JSONArray
import org.json.JSONException
import org.json.JSONObject

/**
 * Version 1 PC list JSON, with optional lastSeen and problem connection metadata. Older lists
 * without these fields decode with the [PcEntry] defaults.
 * Pure Kotlin over org.json, so the round trip runs in JVM tests. Decoding never throws: text of any
 * other shape gives an empty list, and one malformed entry is skipped without losing the others.
 */
object PcListCodec {
    private const val VERSION = 1
    private const val KEY_PCS = "pcs"

    fun encode(entries: List<PcEntry>): String {
        val array = JSONArray()
        for (entry in entries) {
            array.put(
                JSONObject()
                    .put("id", entry.id)
                    .put("publicKey", entry.publicKey)
                    .put("relay", entry.relay)
                    .put("name", entry.name)
                    .put("paired", entry.paired)
                    .put("lastSeen", entry.lastSeen)
                    .put("problem", entry.problem ?: JSONObject.NULL),
            )
        }
        return JSONObject().put("version", VERSION).put(KEY_PCS, array).toString()
    }

    fun decode(text: String?): List<PcEntry> {
        if (text.isNullOrEmpty()) return emptyList()
        val root = try {
            JSONObject(text)
        } catch (e: JSONException) {
            BpLog.w("PcListCodec", "the stored PC list is not valid JSON and is treated as empty", null, emptyMap(), e)
            return emptyList()
        }
        val array = root.optJSONArray(KEY_PCS) ?: return emptyList()
        return (0 until array.length()).mapNotNull { entry(array.optJSONObject(it)) }
    }

    /** One entry, or null when a field the list cannot do without is missing or empty. */
    private fun entry(json: JSONObject?): PcEntry? {
        if (json == null || !json.has("paired")) return null
        val id = json.optString("id")
        val publicKey = json.optString("publicKey")
        val relay = json.optString("relay")
        if (id.isEmpty() || publicKey.isEmpty() || relay.isEmpty()) return null
        return PcEntry(
            id, publicKey, relay, json.optString("name"), json.optLong("paired"),
            lastSeen = json.optLong("lastSeen", 0L),
            problem = json.opt("problem") as? String,
        )
    }
}
