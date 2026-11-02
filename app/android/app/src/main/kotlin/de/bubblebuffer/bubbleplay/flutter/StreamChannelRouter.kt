package de.bubblebuffer.bubbleplay.flutter

/** The `bubbleplay/stream` method calls, away from the Flutter classes. */
internal class StreamChannelRouter(
    private val input: StreamInput,
    private val textureId: () -> Int?,
) {
    /** The result of [method], or [NOT_IMPLEMENTED]. */
    fun handle(method: String, args: Map<String, Any?>?): Any? = when (method) {
        "textureId" -> textureId()
        // Dart reports where the video sits; Android maps pointer input from the area it sends
        // with every event, so the rectangle is not needed.
        "videoRect" -> null
        "pointer" -> {
            args?.let(input::pointer)
            null
        }
        "key" -> {
            if (args != null) {
                input.key((args["usage"] as? Number)?.toInt() ?: 0, args["down"] as? Boolean ?: false)
            }
            null
        }
        else -> NOT_IMPLEMENTED
    }

    companion object {
        val NOT_IMPLEMENTED = Any()
    }
}
