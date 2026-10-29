package de.bubblebuffer.bubbleplay.ui

import java.text.SimpleDateFormat
import java.util.Locale
import java.util.TimeZone
import org.junit.Assert.*
import org.junit.Test

class PcStatusLineTest {
    private val zone = TimeZone.getTimeZone("UTC")
    private fun fmt(p: String) = SimpleDateFormat(p, Locale.ENGLISH).apply { timeZone = zone }
    private val texts = PcStatusLine.Texts(
        neverConnected = "Not connected yet",
        lastSeen = "Last seen %1\$s",
        today = "today, %1\$s",
        withProblem = "%1\$s \u00B7 %2\$s",
        time = { fmt("HH:mm").format(it) },
        date = { fmt("d MMM").format(it) },
    )
    private fun at(day: Int, h: Int, m: Int): Long =
        java.util.Calendar.getInstance(zone).apply { clear(); set(2026, java.util.Calendar.OCTOBER, day, h, m) }.timeInMillis

    @Test fun neverSeen() {
        val l = PcStatusLine.line(0, null, at(28, 15, 0), zone, texts)
        assertEquals(PcStatusLine.Line("Not connected yet", false), l)
    }

    @Test fun seenToday() {
        val l = PcStatusLine.line(at(28, 14, 2), null, at(28, 15, 0), zone, texts)
        assertEquals(PcStatusLine.Line("Last seen today, 14:02", false), l)
    }

    @Test fun seenOtherDay() {
        val l = PcStatusLine.line(at(26, 9, 0), null, at(28, 15, 0), zone, texts)
        assertEquals(PcStatusLine.Line("Last seen 26 Oct", false), l)
    }

    @Test fun withProblemIsBad() {
        val l = PcStatusLine.line(at(26, 9, 0), "The relay refused this device", at(28, 15, 0), zone, texts)
        assertEquals(PcStatusLine.Line("Last seen 26 Oct \u00B7 The relay refused this device", true), l)
    }
}
