package de.bubblebuffer.bubbleplay.log

import org.json.JSONObject
import java.io.BufferedInputStream
import java.io.ByteArrayOutputStream
import java.io.File
import java.io.InputStream
import java.util.PriorityQueue

/** One stored line with its upload cursor. [line] is exactly the stored text, without the newline. */
internal class CursorLine(val cursor: String, val ts: String, val line: String, val bytes: Int)

/**
 * Upload cursors for the Kotlin core's log files (docs/architecture/client-log-upload.md).
 *
 * The cursor of a line is `<ts>#<n>`: the line's own `ts` (24 characters, fixed width) and `n`, a six
 * digit zero-padded counter that numbers the lines sharing that exact `ts` over all files, ordered by file
 * path and then by position in the file. The text is fixed width, so a byte-wise compare is the line order.
 *
 * It is derived from the files, so nothing extra is stored and the stored lines stay unchanged. Known
 * limit: two lines written in the same millisecond into different files could sort against the order they
 * were written in, and a line that sorts before the last sent cursor is not sent. Deleting old day files
 * does not change the cursor of any later line, because ties only happen inside one day.
 */
internal object LogCursor {
    const val COUNTER_DIGITS = 6
    const val MAX_COUNTER = 999_999
    private const val TS_START = 7 // after {"ts":"
    private const val TS_LEN = 24
    private val PREFIX = "{\"ts\":\""

    fun make(ts: String, n: Int): String = ts + "#" + n.coerceIn(0, MAX_COUNTER).toString().padStart(COUNTER_DIGITS, '0')

    /** The `ts` text of a stored line, or null when the line does not start like one of ours. */
    fun tsOf(line: String): String? {
        if (line.length < TS_START + TS_LEN + 1 || !line.startsWith(PREFIX) || line[TS_START + TS_LEN] != '"') return null
        return line.substring(TS_START, TS_START + TS_LEN)
    }

    private class Source(val order: Int, val file: File) {
        private val input: InputStream? = try {
            BufferedInputStream(file.inputStream(), 64 * 1024)
        } catch (_: Throwable) {
            null
        }
        var line: String? = null
        var ts: String = ""

        fun advance() {
            line = null
            val stream = input ?: return
            while (true) {
                val text = nextLine(stream) ?: return
                val t = tsOf(text) ?: continue
                line = text
                ts = t
                return
            }
        }

        fun close() {
            try {
                input?.close()
            } catch (_: Throwable) {
            }
        }
    }

    /** A complete (newline terminated) line, or null at the end; a half-written last line is ignored. */
    private fun nextLine(input: InputStream): String? {
        val out = ByteArrayOutputStream(256)
        try {
            while (true) {
                val b = input.read()
                if (b < 0) return null
                if (b == '\n'.code) return out.toString(Charsets.UTF_8.name())
                out.write(b)
            }
        } catch (_: Throwable) {
            return null
        }
    }

    private fun dayOf(file: File): String =
        Regex("(\\d{4}-\\d{2}-\\d{2})\\.jsonl$").find(file.name)?.groupValues?.get(1) ?: ""

    /**
     * Lines of `root/misc` and `root/pc/<id>` after cursor [after] ("" = from the start) whose `pc` is
     * [pcId] or null, oldest first, at most [maxLines] lines and [maxBytes] UTF-8 bytes (one line
     * minimum). Never throws.
     */
    fun readAfter(root: File, after: String, pcId: String?, maxLines: Int, maxBytes: Int): List<CursorLine> {
        val out = ArrayList<CursorLine>()
        var sources: List<Source> = emptyList()
        try {
            val firstDay = if (after.length >= 10) after.substring(0, 10) else ""
            val files = ArrayList<File>()
            File(root, "misc").listFiles()?.let { files.addAll(it) }
            File(root, "pc").listFiles()?.filter { it.isDirectory }?.forEach { dir -> dir.listFiles()?.let { files.addAll(it) } }
            val chosen = files
                .filter { it.isFile && it.name.endsWith(".jsonl") && dayOf(it) >= firstDay }
                .sortedBy { it.path }
            sources = chosen.mapIndexed { i, f -> Source(i, f) }
            val heap = PriorityQueue<Source>(compareBy<Source>({ it.ts }, { it.order }))
            for (s in sources) {
                s.advance()
                if (s.line != null) heap.add(s)
            }
            var lastTs = ""
            var counter = 0
            var bytes = 0
            while (heap.isNotEmpty() && out.size < maxLines) {
                val s = heap.poll()!!
                val text = s.line!!
                val ts = s.ts
                counter = if (ts == lastTs) counter + 1 else 0
                lastTs = ts
                s.advance()
                if (s.line != null) heap.add(s)
                val cursor = make(ts, counter)
                if (cursor <= after) continue
                val size = text.toByteArray(Charsets.UTF_8).size
                val pc = pcOf(text) ?: continue
                if (pc.isNotEmpty() && pc != pcId) continue
                if (out.isNotEmpty() && bytes + size > maxBytes) break
                out.add(CursorLine(cursor, ts, text, size))
                bytes += size
            }
        } catch (_: Throwable) {
        } finally {
            sources.forEach { it.close() }
        }
        return out
    }

    /** The `pc` of a line: "" for none, null when the line is not valid JSON. */
    private fun pcOf(line: String): String? = try {
        val o = JSONObject(line)
        if (o.isNull("pc")) "" else o.optString("pc", "")
    } catch (_: Throwable) {
        null
    }
}
