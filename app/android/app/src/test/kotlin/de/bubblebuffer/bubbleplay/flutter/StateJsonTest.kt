package de.bubblebuffer.bubbleplay.flutter

import org.json.JSONObject
import org.junit.Assert.assertEquals
import org.junit.Assert.assertTrue
import org.junit.Test
import java.io.File

/**
 * The builders produce the key sets of `app/test/fixtures/contract/states.json`, which the Rust
 * serde types generated, so one Dart model reads both platforms.
 */
class StateJsonTest {
    private val states: JSONObject by lazy {
        val dir = System.getProperty("bubbleplay.contract") ?: error("bubbleplay.contract is not set")
        JSONObject(File(dir, "states.json").readText())
    }

    private fun all(): List<JSONObject> = states.keys().asSequence().map { states.getJSONObject(it) }.toList()

    private fun keysOf(json: JSONObject): Set<String> = json.keys().asSequence().toSet()

    private fun flows(): List<Any> = all().filter { !it.isNull("flow") }.map { it.get("flow") }

    @Test
    fun theFixturesAreThere() {
        assertTrue(states.length() >= 33)
    }

    @Test
    fun topLevelStateHasTheFixtureFields() {
        val ours = StateJson.state("id", "path", emptyList(), null, null)
        assertEquals(keysOf(all().first()), keysOf(ours))
    }

    @Test
    fun aPcHasExactlyTheFieldsOfTheRustPc() {
        val ours = StateJson.pc("77VH-7M56-8GJK-Y12J", "Studio", "https://relay.example", null)
        val theirs = all().first { it.getJSONArray("pcs").length() > 0 }.getJSONArray("pcs").getJSONObject(0)
        assertEquals(keysOf(theirs), keysOf(ours))
    }

    @Test
    fun pcProblemsUseTheFixtureNamesAndUnknownOnesAreNull() {
        val fixture = all().flatMap { s -> (0 until s.getJSONArray("pcs").length()).map { s.getJSONArray("pcs").getJSONObject(it) } }
            .mapNotNull { it.optString("problem").takeIf { p -> p.isNotEmpty() && p != "null" } }.toSet()
        val ours = listOf("session_pc_key_changed", "session_relay_refused", "session_device_removed", "session_auth_timeout")
            .mapNotNull { StateJson.pcProblem(it) }.toSet()
        assertEquals(fixture, ours)
        assertEquals(null, StateJson.pcProblem("session_control_failed"))
        assertEquals(null, StateJson.pcProblem(null))
        assertEquals("KeyChanged", StateJson.pc("a", "b", "r", null, "KeyChanged").getString("problem"))
        assertTrue(StateJson.pc("a", "b", "r", null).isNull("problem"))
    }

    @Test
    fun theSessionHasExactlyTheFieldsOfTheRustSession() {
        val ours = StateJson.session("77VH-7M56-8GJK-Y12J", "Studio", "Work", "Auto", false)
        val theirs = all().first { !it.isNull("session") }.getJSONObject("session")
        assertEquals(keysOf(theirs), keysOf(ours))
        assertEquals(keysOf(theirs.getJSONObject("stats")), keysOf(ours.getJSONObject("stats")))
    }

    @Test
    fun theFlowBuildersMatchTheFixtureVariantsAndFields() {
        fun inner(flow: Any, tag: String): JSONObject? = (flow as? JSONObject)?.optJSONObject(tag)

        val confirm = flows().firstNotNullOf { inner(it, "PairConfirm") }
        assertEquals(
            keysOf(confirm),
            keysOf(StateJson.pairConfirm("a", "b", 1, 2, false, "Waiting").getJSONObject("PairConfirm")),
        )
        val enter = flows().firstNotNullOf { inner(it, "PairEnter") }
        assertEquals(keysOf(enter), keysOf(StateJson.pairEnter(false, null).getJSONObject("PairEnter")))

        val steps = flows().mapNotNull { (it as? JSONObject)?.optJSONObject("Enroll") }
        val unlock = steps.firstNotNullOf { it.optJSONObject("ImportUnlock") }
        val builtUnlock = StateJson.importUnlock("f", null).getJSONObject("Enroll").getJSONObject("ImportUnlock")
        assertEquals(keysOf(unlock), keysOf(builtUnlock))
        val refused = steps.firstNotNullOf { it.optJSONObject("ImportRefused") }
        val builtRefused = StateJson.importRefused("f", "r").getJSONObject("Enroll").getJSONObject("ImportRefused")
        assertEquals(keysOf(refused), keysOf(builtRefused))

        val plain = flows().filterIsInstance<JSONObject>().mapNotNull { it.optString("Enroll").takeIf(String::isNotEmpty) }
        assertTrue(plain.containsAll(listOf("Choose", "Created")))
        assertEquals("Choose", StateJson.choose.getString("Enroll"))
        assertEquals("Created", StateJson.created.getString("Enroll"))
    }
}
