package de.bubblebuffer.bubbleplay

import org.json.JSONArray
import org.json.JSONObject
import java.io.File

/** Shared contract files from <repo>/fixtures plus generated test resources. */
object Fixtures {
    private val dir: File by lazy {
        val configured = System.getProperty("bubbleplay.fixtures")
        val candidates = listOfNotNull(configured?.let(::File), File("../../fixtures"), File("../fixtures"))
        candidates.firstOrNull { File(it, "wss-v4-contract.json").isFile }
            ?: error("fixtures directory not found (bubbleplay.fixtures=$configured)")
    }

    fun json(name: String) = JSONObject(File(dir, name).readText())

    fun resource(name: String): JSONObject {
        val stream = Fixtures::class.java.classLoader!!.getResourceAsStream(name) ?: error("missing test resource $name")
        return JSONObject(stream.bufferedReader().readText())
    }

    fun hex(value: String): ByteArray = ByteArray(value.length / 2) { i -> value.substring(i * 2, i * 2 + 2).toInt(16).toByte() }

    fun bytes(array: JSONArray): ByteArray = ByteArray(array.length()) { i -> array.getInt(i).toByte() }

    fun objects(array: JSONArray): List<JSONObject> = (0 until array.length()).map { array.getJSONObject(it) }
}
