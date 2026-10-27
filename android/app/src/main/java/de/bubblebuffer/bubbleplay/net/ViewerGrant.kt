package de.bubblebuffer.bubbleplay.net

import de.bubblebuffer.bubbleplay.auth.ViewerHello

/** A signaling session's ticket, held in memory and never placed in a URL. */
class ViewerGrant private constructor(val pc: String?, private val ticket: String?) {
    fun hello(role: String): String = ViewerHello.dataHello(role, requireNotNull(ticket))
    override fun toString() = "ViewerGrant(pc=$pc, ticket=${if (ticket == null) "none" else "<redacted>"})"

    companion object {
        val NONE = ViewerGrant(null, null)
        fun from(pc: String, ticket: String): ViewerGrant {
            require(pc.isNotEmpty() && ticket.isNotEmpty())
            return ViewerGrant(pc, ticket)
        }
    }
}
