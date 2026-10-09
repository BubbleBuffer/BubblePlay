package de.bubblebuffer.bubbleplay.net

/** Socket callbacks and retry timers belong to one media leg, including null-socket teardown. */
internal class MediaLeg {
    var id = 0
        private set
    private var hostStartedId: Int? = null

    fun invalidate() { id++; hostStartedId = null }
    fun open(): Int { id++; return id }
    fun startHost(): Boolean {
        if (hostStartedId == id) return false
        hostStartedId = id
        return true
    }
}

/**
 * One forced-IDR retry and one media restart per attached decoder surface.
 * Misses count only while signaling is up: a slow host or relay reconnect is
 * not a decoder-surface failure.
 */
internal class SurfaceRecovery {
    /** WAIT: signaling is down; check again later without counting a miss. */
    enum class Action { NONE, WAIT, KEYFRAME, RESTART, EXHAUSTED }
    private var generation = -1L
    private var waiting = false
    private var misses = 0
    var exhausted = false
        private set

    /** All automatic media opens/restarts use the same durable surface limit. */
    fun canRestartMedia() = !exhausted
    fun waitingGeneration(): Long? = if (waiting) generation else null

    fun changed(generation: Long, attached: Boolean) {
        if (attached && this.generation != generation) exhausted = false
        this.generation = generation
        waiting = attached && !exhausted
        misses = 0
    }

    fun output(generation: Long): Boolean {
        if (this.generation != generation || !waiting) return false
        waiting = false
        return true
    }

    fun missed(generation: Long, signalingUp: Boolean = true): Action {
        if (this.generation != generation || !waiting) return Action.NONE
        // Counting restarts once signaling is back, so each step gets its full window.
        if (!signalingUp) { misses = 0; return Action.WAIT }
        return when (++misses) {
            1 -> Action.KEYFRAME
            2 -> Action.RESTART
            else -> { waiting = false; exhausted = true; Action.EXHAUSTED }
        }
    }
}

/** Presentation and signaling admission may arrive in either order for a media leg. */
internal class FirstFramePresentation {
    private var requestId: String? = null
    private var mediaLeg = -1
    private var requireAdmission = false
    private var observedGeneration: Long? = null
    var generation = 0L
        private set
    var admitted = false
        private set
    var presented = false
        private set

    fun start(requestId: String, mediaLeg: Int, requireAdmission: Boolean) {
        reset()
        this.requestId = requestId
        this.mediaLeg = mediaLeg
        this.requireAdmission = requireAdmission
    }

    fun reset() {
        requestId = null
        mediaLeg = -1
        observedGeneration = null
        generation = 0
        admitted = false
        presented = false
    }

    /** True only on the transition that opens input for this request. */
    fun observe(requestId: String, mediaLeg: Int, generation: Long): Boolean {
        if (this.requestId != requestId || this.mediaLeg != mediaLeg) return false
        observedGeneration = generation
        return complete()
    }

    fun admit(requestId: String, generation: Long): Boolean {
        if (this.requestId != requestId) return false
        admitted = true
        this.generation = generation
        return complete()
    }

    private fun complete(): Boolean {
        val observed = observedGeneration ?: return false
        if (presented || (requireAdmission && !admitted)) return false
        if (requireAdmission && generation != 0L && observed != generation) return false
        presented = true
        return true
    }
}
