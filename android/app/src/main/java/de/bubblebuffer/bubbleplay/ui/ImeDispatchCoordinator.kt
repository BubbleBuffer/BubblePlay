package de.bubblebuffer.bubbleplay.ui

/** Orders real IME dispatch and retires editor context without restarting inside a callback. */
class ImeDispatchCoordinator(
    private val owns: (Int) -> Boolean,
    private val flush: (Int) -> Boolean,
    private val clearLocal: (Boolean) -> Boolean,
    private val restart: () -> Unit,
    private val post: (() -> Unit) -> Unit,
) {
    enum class Action { DOWN, UP, MULTIPLE, OTHER }
    private data class Press(val resetAfter: Boolean, val restartAfter: Boolean)
    private val presses = HashMap<Pair<Int, Int>, Press>()
    private var depth = 0
    private var revision = 0L
    private var pendingClear = false
    private var pendingClearRestart = false
    private var pendingRestart = false
    private var postedRestartRevision: Long? = null
    val resetRequested get() = pendingClear
    val inOperation get() = depth > 0

    fun beginOperation(editorEdit: Boolean = false) {
        if (editorEdit && depth == 0 && pendingClear) drain()
        if (editorEdit) editorEdited()
        depth++
    }

    fun endOperation() {
        check(depth > 0)
        depth--
        if (depth == 0) drain()
    }

    /** A later edit cancels only obsolete posted restart work, never that edit's text. */
    fun editorEdited() {
        revision++
        pendingRestart = false
        postedRestartRevision = null
    }

    fun connectionChanged() {
        editorEdited()
        pendingClear = false
        pendingClearRestart = false
    }

    /** Sink callbacks can request retirement, which drains after reconciliation has returned. */
    fun requestReset(restartNeeded: Boolean = true) {
        pendingClear = true
        pendingClearRestart = pendingClearRestart || restartNeeded
        if (depth == 0) {
            val epoch = revision
            post { if (revision == epoch && pendingClear && depth == 0) drain() }
        }
    }

    fun externalReset() {
        if (inOperation) { requestReset(); return }
        editorEdited()
        pendingClear = false
        pendingClearRestart = false
        clearLocal(true)
        restart() // Hardware/QuickBar/external resets retain their existing synchronous behavior.
    }

    private fun drain() {
        if (pendingClear) {
            pendingClear = false
            revision++
            val needed = clearLocal(false)
            pendingRestart = (needed && pendingClearRestart) || pendingRestart
            pendingClearRestart = false
        }
        scheduleRestart()
    }

    private fun scheduleRestart() {
        if (!pendingRestart || presses.any { owns(it.key.first) && it.value.resetAfter }) return
        if (postedRestartRevision == revision) return
        val epoch = revision
        postedRestartRevision = epoch
        post {
            if (revision == epoch && postedRestartRevision == epoch && pendingRestart && depth == 0 &&
                presses.none { owns(it.key.first) && it.value.resetAfter }) {
                pendingRestart = false
                postedRestartRevision = null
                restart()
            }
        }
    }

    fun dispatch(connection: Int, key: Int, action: Action, resetAfter: Boolean,
        restartAfter: Boolean = resetAfter, deduplicateDown: Boolean = false, send: () -> Boolean): Boolean {
        beginOperation()
        try {
            val identity = connection to key
            val existing = presses[identity]
            if (!owns(connection)) {
                // Only release a key sent by this old connection; do not touch the new editor.
                return if (action == Action.UP && existing != null) {
                    presses.remove(identity)
                    send()
                } else false
            }
            if (action == Action.UP && existing == null) return true
            if (deduplicateDown && existing != null && (action == Action.DOWN || action == Action.MULTIPLE)) return true
            val flushed = flush(connection)
            // Never strand an accepted DOWN because later text reconciliation is gated.
            if (action != Action.UP && (!flushed || !owns(connection))) return false
            if (action == Action.UP && !owns(connection)) {
                presses.remove(identity)
                return send()
            }
            val accepted = send()
            when (action) {
                Action.DOWN -> if (accepted) presses[identity] = Press(resetAfter, restartAfter)
                Action.UP -> {
                    presses.remove(identity)
                    if (existing?.resetAfter == true) requestReset(existing.restartAfter)
                }
                Action.MULTIPLE -> if (accepted && resetAfter) requestReset(restartAfter)
                Action.OTHER -> Unit
            }
            return accepted
        } finally { endOperation() }
    }

    /** Explicit editor action, one down/up pair; no inherited synthesized default. */
    fun editorAction(connection: Int, enterKey: Int, down: () -> Boolean, up: () -> Boolean): Boolean {
        beginOperation()
        try {
            if (!dispatch(connection, enterKey, Action.DOWN, resetAfter = true,
                    deduplicateDown = true, send = down)) return false
            return dispatch(connection, enterKey, Action.UP, resetAfter = true, send = up)
        } finally { endOperation() }
    }

    companion object {
        /** Raw printable/Space packets retire their unknown tail, without restarting the IME. */
        fun needsRestart(code: String?): Boolean = code in setOf("Enter", "NumpadEnter", "Tab",
            "ArrowLeft", "ArrowRight", "ArrowUp", "ArrowDown", "Home", "End", "PageUp", "PageDown")
    }
}
