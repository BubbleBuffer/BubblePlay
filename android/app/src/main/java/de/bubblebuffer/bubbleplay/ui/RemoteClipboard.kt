package de.bubblebuffer.bubbleplay.ui

/** A host snapshot waits until the stream is foreground; local clipboard actions cancel it. */
class RemoteClipboard {
    data class Row(val enabled: Boolean, val characters: Int)

    var foreground = false
    var automatic = true
    var pending: String? = null
        private set
    private var baselineKnown = false
    private var clipboardTimestamp: Long? = null

    val row: Row get() {
        val text = pending
        return Row(text != null, text?.codePointCount(0, text.length) ?: 0)
    }

    fun receive(text: String) { pending = text }
    fun localAction() { pending = null }

    fun observeLocal(timestamp: Long?) {
        if (baselineKnown && timestamp != clipboardTimestamp) localAction()
        rememberWrite(timestamp)
    }

    fun rememberWrite(timestamp: Long?) {
        clipboardTimestamp = timestamp
        baselineKnown = true
    }

    fun takeAutomatic(): String? {
        if (!foreground || !automatic) return null
        return take()
    }

    fun take(): String? {
        if (!foreground) return null
        val text = pending
        pending = null
        return text
    }
}
