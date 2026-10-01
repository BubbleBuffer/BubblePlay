package de.bubblebuffer.bubbleplay.files

import org.junit.Assert.assertEquals
import org.junit.Assert.assertTrue
import org.junit.Test

class FileNamesTest {
    private fun utf8(s: String) = s.toByteArray(Charsets.UTF_8).size

    @Test
    fun downloadNamesAreSafeForMediaStore() {
        assertEquals("report.pdf", FileNames.forDownload("report.pdf"))
        assertEquals("evil.txt", FileNames.forDownload("..\\..\\Windows\\evil.txt"))
        assertEquals("passwd", FileNames.forDownload("/etc/passwd"))
        assertEquals("a_b_c_d.txt", FileNames.forDownload("a:b*c?d.txt"))
        assertEquals("tab_name.txt", FileNames.forDownload("tab\tname.txt"))
        assertEquals("hidden", FileNames.forDownload(".hidden"))
        assertEquals("trailing", FileNames.forDownload("trailing. . "))
        assertEquals(FileNames.FALLBACK_DOWNLOAD, FileNames.forDownload(""))
        assertEquals(FileNames.FALLBACK_DOWNLOAD, FileNames.forDownload("..."))
        assertEquals(FileNames.FALLBACK_DOWNLOAD, FileNames.forDownload("C:\\dir\\"))
        assertEquals(FileNames.FALLBACK_DOWNLOAD, FileNames.forDownload("???"))
        assertEquals("Café 日本.txt", FileNames.forDownload("Café 日本.txt"))
    }

    @Test
    fun longNamesKeepTheirExtensionWithin255Bytes() {
        val long = "日".repeat(200) + ".jpeg"
        val fitted = FileNames.forDownload(long)
        assertTrue(utf8(fitted) <= 255)
        assertTrue(fitted.endsWith(".jpeg"))
        assertTrue(fitted.startsWith("日"))
        // Surrogate pairs are never split.
        val emoji = FileNames.forDownload("😀".repeat(100) + ".png")
        assertTrue(utf8(emoji) <= 255)
        assertTrue(emoji.removeSuffix(".png").all { it.isSurrogate() })
        assertEquals(0, emoji.removeSuffix(".png").length % 2)
    }

    @Test
    fun uploadNames() {
        assertEquals("IMG_1.jpg", FileNames.forUpload("IMG_1.jpg"))
        assertEquals("x.txt", FileNames.forUpload("dir/sub/x.txt"))
        assertEquals(FileNames.FALLBACK_UPLOAD, FileNames.forUpload(null))
        assertEquals(FileNames.FALLBACK_UPLOAD, FileNames.forUpload("  "))
        val long = FileNames.forUpload("a".repeat(400) + ".mp4")
        assertEquals(255, long.length)
        assertTrue(long.endsWith(".mp4"))
    }

    @Test
    fun mimeTypes() {
        val table = mapOf("png" to "image/png", "pdf" to "application/pdf")
        val guess = { ext: String -> table[ext] }
        assertEquals("image/jpeg", FileNames.mime("a.jpg", "image/jpeg", guess))
        assertEquals("image/png", FileNames.mime("a.PNG", "", guess))
        assertEquals("image/png", FileNames.mime("a.png", "not a mime", guess))
        assertEquals("application/pdf", FileNames.mime("a.pdf", null, guess))
        assertEquals(FileNames.OCTET_STREAM, FileNames.mime("a.weird", null, guess))
        assertEquals(FileNames.OCTET_STREAM, FileNames.mime("noext", null, guess))
        assertEquals("text/plain", FileNames.mime("a", "Text/Plain", guess))
    }

    @Test
    fun shareIntentParsing() {
        val send = ShareParse.ACTION_SEND
        val multi = ShareParse.ACTION_SEND_MULTIPLE
        assertEquals(listOf("a"), ShareParse.collect(send, "a", null, emptyList()).items)
        // Only ClipData filled (some apps): falls back to it.
        assertEquals(listOf("c1", "c2"), ShareParse.collect(send, null, null, listOf("c1", null, "c2")).items)
        assertEquals(listOf("a", "b"), ShareParse.collect(multi, null, listOf("a", null, "b", "a"), listOf("x")).items)
        assertEquals(emptyList<String>(), ShareParse.collect(send, null, null, emptyList<String>()).items)
        assertEquals(emptyList<String>(), ShareParse.collect("android.intent.action.VIEW", "a", null, emptyList()).items)
        val many = (1..70).map { "u$it" }
        val result = ShareParse.collect(multi, null, many, emptyList())
        assertEquals(64, result.items.size)
        assertEquals(6, result.dropped)
        assertEquals("u1", result.items.first())
    }
}
