package de.bubblebuffer.bubbleplay.protocol

/** Semantic host cursor IDs shared by `fixtures/cursor-types.json`. No image data travels. */
enum class CursorKind(val wireId: Int) {
    Arrow(0), Text(1), Pointer(2), Crosshair(3), Move(4),
    ResizeNS(5), ResizeEW(6), ResizeNESW(7), ResizeNWSE(8),
    Wait(9), Progress(10), NotAllowed(11), Help(12), UpArrow(13), Hidden(14);

    companion object {
        fun fromWire(id: Int): CursorKind = entries.firstOrNull { it.wireId == id } ?: Arrow
    }
}

/** Host -> client cursor appearance (type 129): exactly one u8 semantic kind. */
object CursorType {
    const val PACKET_TYPE = 129
    const val PACKET_BYTES = 2

    fun decode(bytes: ByteArray): CursorKind? {
        if (bytes.size != PACKET_BYTES || (bytes[0].toInt() and 0xff) != PACKET_TYPE) return null
        return CursorKind.fromWire(bytes[1].toInt() and 0xff)
    }
}
