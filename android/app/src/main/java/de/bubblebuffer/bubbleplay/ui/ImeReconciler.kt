package de.bubblebuffer.bubbleplay.ui

/** Tracks the trailing segment whose caret is known to be at the host's end. */
class ImeReconciler(private val emit: (TextEdit.Edit, Int) -> Boolean) {
    var remoteText: String = ""
        private set
    var localText: String = ""
        private set
    var batchDepth: Int = 0
        private set
    private var extraBackspaces = 0
    private var forwardDeletes = 0
    private var generation = 0
    private var connectionGeneration = 0
    val hasPendingEdits: Boolean
        get() = localText != remoteText || extraBackspaces > 0 || forwardDeletes > 0

    /** A replacement connection owns fresh nesting, while retaining known text context. */
    fun openConnection(): Int {
        connectionGeneration++
        batchDepth = 0
        return connectionGeneration
    }

    fun ownsConnection(connection: Int) = connection == connectionGeneration

    fun closeConnection(connection: Int) {
        if (!ownsConnection(connection)) return
        connectionGeneration++
        batchDepth = 0
    }

    fun abortBatches(connection: Int) {
        if (ownsConnection(connection)) batchDepth = 0
    }

    fun beginBatch(connection: Int? = null): Boolean {
        if (connection != null && !ownsConnection(connection)) return false
        batchDepth++
        return true
    }

    fun endBatch(reconcile: Boolean = true, connection: Int? = null): Boolean {
        if (connection != null && !ownsConnection(connection)) return false
        if (batchDepth == 0) return false
        batchDepth--
        return if (batchDepth == 0 && reconcile) reconcile() else true
    }

    fun changed(text: String, backspacesOutsideContext: Int = 0, deletesOutsideContext: Int = 0, connection: Int? = null): Boolean {
        if (connection != null && !ownsConnection(connection)) return false
        localText = text
        extraBackspaces += backspacesOutsideContext
        forwardDeletes += deletesOutsideContext
        return if (batchDepth == 0) reconcile() else true
    }

    /** Key packets are an ordering boundary even inside a nested IME batch. */
    fun beforeKeyEvent(connection: Int): Boolean {
        return ownsConnection(connection) && reconcile() && ownsConnection(connection)
    }

    /** Editor-handled delete keys flush ordering without discarding suggestions or nesting. */
    fun beforeDeleteKey(connection: Int): Boolean =
        ownsConnection(connection) && reconcile() && ownsConnection(connection)

    fun deletedByKey(text: String, outsideBackspaces: Int, outsideForwardDeletes: Int, connection: Int): Boolean {
        val accepted = changed(text, outsideBackspaces, outsideForwardDeletes, connection)
        return if (accepted && batchDepth > 0) reconcile() else accepted
    }

    fun reconcile(): Boolean {
        // A UTF-16-unit deletion can temporarily leave half a surrogate pair.
        // Wait for a complete edit rather than let UTF-8 encoding replace it with '?'.
        if (!wellFormedUtf16(localText)) return false
        val edit = if (extraBackspaces > 0) {
            // Crossing the retained prefix removes the old segment completely.
            // A new local suffix can resemble it without sharing any host prefix.
            TextEdit.Edit(remoteText.codePointCount(0, remoteText.length) + extraBackspaces, localText)
        } else TextEdit.reconcile(remoteText, localText)
        if (edit.deleteCount == 0 && edit.insert.isEmpty() && forwardDeletes == 0) return true
        val epoch = generation
        val target = localText
        if (!emit(edit, forwardDeletes)) return false
        // A partial-send handler may have reset the context inside emit().
        if (epoch != generation) return false
        remoteText = target
        extraBackspaces = 0
        forwardDeletes = 0
        return true
    }

    /** Drop only a prefix already mirrored successfully; no host edit is needed. */
    fun trimPrefix(count: Int) {
        check(!hasPendingEdits)
        localText = localText.substring(count)
        remoteText = remoteText.substring(count)
    }

    fun reset(invalidateConnection: Boolean = true, preserveBatch: Boolean = false) {
        generation++
        if (invalidateConnection) connectionGeneration++
        remoteText = ""
        localText = ""
        extraBackspaces = 0
        forwardDeletes = 0
        if (!preserveBatch) batchDepth = 0
    }

    companion object {
        const val MAX_CONTEXT = 2048

        private fun wellFormedUtf16(text: String): Boolean {
            var index = 0
            while (index < text.length) {
                val ch = text[index++]
                if (ch.isHighSurrogate()) {
                    if (index == text.length || !text[index++].isLowSurrogate()) return false
                } else if (ch.isLowSurrogate()) return false
            }
            return true
        }

        /** Prefix length to remove without separating a UTF-16 surrogate pair. */
        fun trimCount(text: String): Int {
            var count = (text.length - MAX_CONTEXT).coerceAtLeast(0)
            if (count > 0 && count < text.length && text[count].isLowSurrogate() && text[count - 1].isHighSurrogate()) count++
            return count
        }

        /** Android deletion APIs count UTF-16 units or Unicode code points, respectively. */
        fun beforeStart(text: String, cursor: Int, count: Int, codePoints: Boolean): Int? {
            if (count < 0) return null
            if (!codePoints) return (cursor - count.coerceAtMost(cursor)).coerceAtLeast(0)
            var index = cursor
            repeat(count.coerceAtMost(cursor)) {
                if (index == 0) return 0
                val last = text[--index]
                if (last.isLowSurrogate()) {
                    if (index == 0 || !text[index - 1].isHighSurrogate()) return null
                    index--
                } else if (last.isHighSurrogate()) return null
            }
            return index
        }
    }
}

/** Keep a common prefix and rewrite the trailing text using code-point Backspaces. */
object TextEdit {
    data class Edit(val deleteCount: Int, val insert: String)

    fun reconcile(previous: String, next: String): Edit {
        val before = previous.codePoints().toArray()
        val after = next.codePoints().toArray()
        var prefix = 0
        while (prefix < before.size && prefix < after.size && before[prefix] == after[prefix]) prefix++
        val insert = StringBuilder()
        for (i in prefix until after.size) insert.appendCodePoint(after[i])
        return Edit(before.size - prefix, insert.toString())
    }
}
