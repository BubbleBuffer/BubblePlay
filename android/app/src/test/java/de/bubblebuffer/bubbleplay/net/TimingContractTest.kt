package de.bubblebuffer.bubbleplay.net

import de.bubblebuffer.bubbleplay.Fixtures
import de.bubblebuffer.bubbleplay.auth.Handshake
import de.bubblebuffer.bubbleplay.protocol.V4
import org.json.JSONArray
import org.json.JSONObject
import org.junit.Assert.assertEquals
import org.junit.Test

/** fixtures/session-timing-contract.json, "kotlin" section: the Android viewer's timers and retry schedules. */
class TimingContractTest {
    private val contract = Fixtures.json("session-timing-contract.json")
    private val values = contract.getJSONObject("kotlin").getJSONObject("values")

    private fun schedule(backoff: JSONObject): List<Long> = longs(contract.getJSONObject("schedules").getJSONArray(backoff.getString("schedule")))

    private fun longs(array: JSONArray) = List(array.length()) { array.getLong(it) }

    @Test
    fun heartbeatKeyframeAndBackoffConstantsMatch() {
        assertEquals(values.getLong("heartbeatMs"), V4.HEARTBEAT_MS)
        assertEquals(values.getLong("keyframeThrottleMs"), V4.KEYFRAME_REQUEST_INTERVAL_MS)
        for (name in listOf("signalingBackoff", "mediaBackoff")) {
            val backoff = values.getJSONObject(name)
            assertEquals(backoff.getLong("minMs"), V4.BACKOFF_MIN_MS)
            assertEquals(backoff.getLong("maxMs"), V4.BACKOFF_MAX_MS)
            assertEquals(2, backoff.getInt("factor"))
            assertEquals(0, backoff.getInt("jitterPct"))
        }
    }

    @Test
    fun reconnectLoopFollowsTheContractSchedule() {
        val expected = schedule(values.getJSONObject("signalingBackoff"))
        assertEquals(expected, schedule(values.getJSONObject("mediaBackoff")))
        val loop = ReconnectLoop()
        val seen = expected.map { loop.schedule().also { loop.fire() } }
        assertEquals(expected, seen)
    }

    @Test
    fun signalingLivenessUsesTheContractLimits() {
        val heartbeat = values.getLong("heartbeatMs")
        val limit = values.getLong("signalingAnswerLimitMs")
        val liveness = SignalLiveness(V4.HEARTBEAT_MS)
        liveness.opened(0)
        assertEquals(SignalLiveness.Action.PING, liveness.check(heartbeat))
        assertEquals(SignalLiveness.Action.NONE, liveness.check(heartbeat + limit))
        assertEquals(SignalLiveness.Action.DEAD, liveness.check(heartbeat + limit + 1))

        val probing = SignalLiveness(V4.HEARTBEAT_MS)
        probing.opened(0)
        probing.input(0)
        assertEquals(SignalLiveness.Action.NONE, probing.check(values.getLong("inputProbeAfterMs") - 1))
        assertEquals(SignalLiveness.Action.PING, probing.check(values.getLong("inputProbeAfterMs")))
    }

    @Test
    fun authHostWaitsAndGraceMatch() {
        assertEquals(values.getLong("authHostTimeoutMs"), Handshake.AUTH_HOST_TIMEOUT_MS)
        assertEquals(values.getLong("recoveryBudgetMs"), Handshake.RECYCLED_RETRY_BUDGET_MS)
        assertEquals(values.getLong("backgroundGraceMs"), BackgroundGrace.GRACE_MS)
        val recycled = values.getJSONObject("recycledAuthHost")
        assertEquals(recycled.getLong("firstMs"), Handshake.RECYCLED_AUTH_HOST_TIMEOUT_MS)
        assertEquals(recycled.getLong("maxMs"), Handshake.RECYCLED_AUTH_HOST_TIMEOUT_MAX_MS)

        val retry = RecycledRetry { 0L }
        assertEquals(values.getLong("authHostTimeoutMs"), retry.authHostTimeoutMs())
        retry.authenticated()
        retry.lost()
        assertEquals(schedule(recycled), List(5) { retry.authHostTimeoutMs() })
    }
}
