package de.bubblebuffer.bubbleplay.files

import de.bubblebuffer.bubbleplay.protocol.FileWire
import java.nio.charset.StandardCharsets

/** File names and MIME types for both directions. Pure (unit tested). */
object FileNames {
    const val FALLBACK_DOWNLOAD = "download"
    const val FALLBACK_UPLOAD = "file"
    const val OCTET_STREAM = "application/octet-stream"
    private const val MAX_BYTES = 255
    private const val MAX_UPLOAD_CHARS = 255
    private const val MAX_EXTENSION = 16
    private val MIME = Regex("^[a-z0-9][a-z0-9!#$&^_.+-]{0,63}/[a-z0-9][a-z0-9!#$&^_.+-]{0,126}$")
    private val ILLEGAL = setOf('"', '*', ':', '<', '>', '?', '\\', '/', '|')

    /**
     * A display name MediaStore can store in Download/BubblePlay: the last
     * path component only, no control or FAT-illegal characters, no leading
     * dots (hidden files) and no trailing dots or spaces, at most 255 UTF-8
     * bytes with the extension kept. MediaStore itself appends " (n)" on a
     * collision.
     */
    fun forDownload(name: String): String {
        val last = name.substringAfterLast('/').substringAfterLast('\\')
        val cleaned = buildString {
            for (ch in last) append(if (ch.code < 0x20 || ch.code == 0x7F || ch in ILLEGAL) '_' else ch)
        }.trim().trimStart('.').trimEnd('.', ' ')
        if (cleaned.isEmpty() || cleaned.all { it == '_' }) return FALLBACK_DOWNLOAD
        return fitBytes(cleaned)
    }

    /** The name sent in OFFER: the provider's display name without any path, capped for the wire. */
    fun forUpload(displayName: String?): String {
        val last = displayName.orEmpty().substringAfterLast('/').substringAfterLast('\\')
        val cleaned = last.filter { it.code >= 0x20 && it.code != 0x7F }.trim()
        if (cleaned.isEmpty()) return FALLBACK_UPLOAD
        // Well inside the wire's 1024 UTF-8 bytes; Windows paths are friendlier with short names.
        return if (cleaned.length <= MAX_UPLOAD_CHARS) cleaned else fitChars(cleaned, MAX_UPLOAD_CHARS)
    }

    fun extension(name: String): String {
        val dot = name.lastIndexOf('.')
        if (dot <= 0 || dot == name.length - 1) return ""
        val ext = name.substring(dot + 1)
        return if (ext.length <= MAX_EXTENSION && ext.all { it.isLetterOrDigit() }) ext.lowercase() else ""
    }

    /** The offered MIME type when it is well formed, else a guess from the extension, else octet-stream. */
    fun mime(name: String, offered: String?, guess: (String) -> String?): String {
        val clean = offered?.trim()?.lowercase()
        if (clean != null && MIME.matches(clean)) return clean
        val ext = extension(name)
        if (ext.isNotEmpty()) guess(ext)?.lowercase()?.takeIf { MIME.matches(it) }?.let { return it }
        return OCTET_STREAM
    }

    private fun utf8(s: String) = s.toByteArray(StandardCharsets.UTF_8).size

    /** Shorten the stem (never the extension) until the name fits [MAX_BYTES] UTF-8 bytes. */
    private fun fitBytes(name: String): String {
        if (utf8(name) <= MAX_BYTES) return name
        val ext = extension(name)
        val suffix = if (ext.isEmpty()) "" else name.substring(name.length - ext.length - 1)
        var stem = name.substring(0, name.length - suffix.length)
        while (stem.isNotEmpty() && utf8(stem) + utf8(suffix) > MAX_BYTES) {
            stem = stem.substring(0, stem.offsetByCodePoints(stem.length, -1))
        }
        stem = stem.trimEnd('.', ' ')
        return if (stem.isEmpty()) FALLBACK_DOWNLOAD + suffix else stem + suffix
    }

    private fun fitChars(name: String, max: Int): String {
        val ext = extension(name)
        val suffix = if (ext.isEmpty()) "" else name.substring(name.length - ext.length - 1)
        var stem = name.substring(0, name.length - suffix.length)
        while (stem.isNotEmpty() && stem.length + suffix.length > max) stem = stem.substring(0, stem.offsetByCodePoints(stem.length, -1))
        return stem + suffix
    }
}

/** Which content URIs a share-sheet intent carries. Generic over the URI type so it runs on the JVM. */
object ShareParse {
    const val ACTION_SEND = "android.intent.action.SEND"
    const val ACTION_SEND_MULTIPLE = "android.intent.action.SEND_MULTIPLE"

    class Result<T>(val items: List<T>, val dropped: Int)

    /** Shared by intent intake and the final upload boundary, before any provider is opened. */
    fun rejectionReason(scheme: String?, authority: String?, applicationId: String): String? = when {
        !scheme.equals("content", ignoreCase = true) -> "non-content scheme"
        authority.isNullOrEmpty() -> "missing authority"
        authority.substringAfterLast('@').let {
            it.equals(applicationId, ignoreCase = true) || it.startsWith("$applicationId.", ignoreCase = true)
        } -> "app-owned authority"
        else -> null
    }

    /**
     * ACTION_SEND uses EXTRA_STREAM, ACTION_SEND_MULTIPLE the stream list;
     * either falls back to the ClipData items (some apps only fill those).
     * Only external content providers are accepted. Duplicates are dropped;
     * more than [max] accepted items keeps the first [max].
     */
    fun <T : Any> collect(
        action: String?, stream: T?, streams: List<T?>?, clip: List<T?>,
        applicationId: String, uriParts: (T) -> Pair<String?, String?>,
        max: Int = FileWire.MAX_BATCH, onRejected: (String) -> Unit = {},
    ): Result<T> {
        val primary: List<T?> = when (action) {
            ACTION_SEND -> listOfNotNull(stream)
            ACTION_SEND_MULTIPLE -> streams.orEmpty()
            else -> return Result(emptyList(), 0)
        }
        val chosen = primary.filterNotNull().ifEmpty { clip.filterNotNull() }
        val unique = LinkedHashSet(chosen).filter { uri ->
            val (scheme, authority) = uriParts(uri)
            val reason = rejectionReason(scheme, authority, applicationId)
            if (reason != null) onRejected(reason)
            reason == null
        }
        return Result(unique.take(max), (unique.size - max).coerceAtLeast(0))
    }
}
