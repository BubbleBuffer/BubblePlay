package de.bubblebuffer.bubbleplay.ui

import java.util.Calendar
import java.util.TimeZone

/** The fourth line of a PC row: when the PC was last reached and its last problem, if any. */
internal object PcStatusLine {
    /** [bad] is true when the line carries a problem and so uses the danger colour. */
    data class Line(val text: String, val bad: Boolean)

    /** Text pieces and date formatters, so the logic stays pure. */
    class Texts(
        val neverConnected: String,
        /** "Last seen %1$s" */
        val lastSeen: String,
        /** "today, %1$s" */
        val today: String,
        /** "%1$s · %2$s" */
        val withProblem: String,
        /** Time of day for a moment, in the device's 12/24 h setting. */
        val time: (Long) -> String,
        /** Short date such as "26 Oct", in the device locale. */
        val date: (Long) -> String,
    )

    fun line(lastSeen: Long, problem: String?, now: Long, zone: TimeZone, texts: Texts): Line {
        val base = if (lastSeen <= 0L) {
            texts.neverConnected
        } else {
            val when_ = if (sameDay(lastSeen, now, zone)) texts.today.format(texts.time(lastSeen)) else texts.date(lastSeen)
            texts.lastSeen.format(when_)
        }
        val text = problem?.takeIf { it.isNotBlank() }
        return if (text == null) Line(base, false) else Line(texts.withProblem.format(base, text), true)
    }

    private fun sameDay(a: Long, b: Long, zone: TimeZone): Boolean {
        val x = Calendar.getInstance(zone).apply { timeInMillis = a }
        val y = Calendar.getInstance(zone).apply { timeInMillis = b }
        return x.get(Calendar.YEAR) == y.get(Calendar.YEAR) && x.get(Calendar.DAY_OF_YEAR) == y.get(Calendar.DAY_OF_YEAR)
    }
}
