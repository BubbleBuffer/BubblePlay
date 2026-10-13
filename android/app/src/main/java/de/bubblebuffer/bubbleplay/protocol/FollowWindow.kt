package de.bubblebuffer.bubbleplay.protocol

import org.json.JSONObject

/**
 * Follow active window (fixtures/follow-window-contract.json): the host
 * advertises `host-capabilities.followWindow` (an integer version), the client
 * sends a `media-control` `follow` boolean on every connect and on a change,
 * and `monitor-list.follow` reports the host's state.
 */
object FollowWindow {
    const val REASON = "user-follow"

    /** `followWindow` is an integer >= 1 (the browser's `Number.isInteger(v) && v >= 1`). */
    fun capable(capabilities: JSONObject): Boolean {
        val value = capabilities.opt("followWindow") as? Number ?: return false
        val d = value.toDouble()
        return !d.isNaN() && !d.isInfinite() && d == Math.floor(d) && d >= 1.0
    }

    fun control(follow: Boolean, epoch: String?): JSONObject = JSONObject()
        .put("type", "media-control").put("action", "follow").put("follow", follow)
        .put("protocol", V4.VERSION).put("epoch", epoch).put("reason", REASON)

    /** The host's follow state from a `monitor-list`, or null when it does not report one. */
    fun listState(monitorList: JSONObject): Boolean? = monitorList.opt("follow") as? Boolean
}
