package de.bubblebuffer.bubbleplay.ui

import android.content.Context
import android.graphics.Typeface
import android.graphics.drawable.GradientDrawable
import android.view.Gravity
import android.view.View
import android.widget.Button
import android.widget.FrameLayout
import android.widget.LinearLayout
import android.widget.TextView
import de.bubblebuffer.bubbleplay.net.SecureDesktop
import kotlin.math.roundToInt

/**
 * Top banner while the PC shows a secure desktop (UAC prompt, lock screen):
 * what is open, what this viewer may do there, and a Deny button when the
 * host's SYSTEM helper offers it. The stream stays visible underneath.
 */
class SecureBanner(context: Context, root: FrameLayout, private val onDeny: () -> Unit) {
    private val density = context.resources.displayMetrics.density
    private var state: SecureDesktop? = null
    private var denyPending = false
    private var denyError = ""

    private val title = TextView(context).apply {
        setTextColor(MILK); textSize = 14f; typeface = Typeface.DEFAULT_BOLD
    }
    private val detail = TextView(context).apply { setTextColor(CREAM); textSize = 12f }
    private val deny = Button(context).apply {
        text = "Deny prompt"
        isAllCaps = false
        setTextColor(INK)
        background = shape(APRICOT)
        minHeight = 0; minimumHeight = 0
        setPadding(dp(12), dp(6), dp(12), dp(6))
        setOnClickListener { if (!denyPending) onDeny() }
    }
    private val view = LinearLayout(context).apply {
        orientation = LinearLayout.HORIZONTAL
        gravity = Gravity.CENTER_VERTICAL
        setPadding(dp(14), dp(8), dp(10), dp(8))
        background = shape(DUVET).apply { setStroke(dp(1), THREAD) }
        visibility = View.GONE
        // Taps on the banner never reach the stream's touch handling.
        isClickable = true
        addView(LinearLayout(context).apply {
            orientation = LinearLayout.VERTICAL
            addView(title); addView(detail)
        }, LinearLayout.LayoutParams(0, LinearLayout.LayoutParams.WRAP_CONTENT, 1f))
        addView(deny, LinearLayout.LayoutParams(LinearLayout.LayoutParams.WRAP_CONTENT, LinearLayout.LayoutParams.WRAP_CONTENT).apply {
            marginStart = dp(10)
        })
    }

    init {
        root.addView(view, FrameLayout.LayoutParams(FrameLayout.LayoutParams.MATCH_PARENT, FrameLayout.LayoutParams.WRAP_CONTENT, Gravity.TOP).apply {
            topMargin = dp(36); leftMargin = dp(8); rightMargin = dp(8)
        })
    }

    fun show(next: SecureDesktop?) {
        if (next?.kind != state?.kind) { denyPending = false; denyError = "" }
        state = next
        render()
    }

    fun denySent() { denyPending = true; denyError = ""; render() }

    fun denyResult(ok: Boolean, reason: String) {
        denyPending = false
        denyError = if (ok) "" else reason.ifEmpty { "failed" }
        render()
    }

    private fun render() {
        val s = state
        if (s == null) { view.visibility = View.GONE; return }
        val (t, d) = text(s, denyPending, denyError)
        title.text = t
        detail.text = d
        deny.visibility = if ("deny" in s.capabilities) View.VISIBLE else View.GONE
        deny.isEnabled = !denyPending
        deny.alpha = if (denyPending) 0.6f else 1f
        view.visibility = View.VISIBLE
    }

    private fun shape(fill: Int) = GradientDrawable().apply {
        cornerRadii = floatArrayOf(9f, 9f, 13f, 13f, 8f, 8f, 11f, 11f).map { it * density }.toFloatArray()
        setColor(fill)
    }

    private fun dp(value: Int) = (value * density).roundToInt()

    companion object {
        /** Banner title and detail; same wording as the browser receiver. */
        fun text(s: SecureDesktop, denyPending: Boolean, denyError: String): Pair<String, String> {
            val title = when (s.kind) {
                "uac" -> "Admin prompt open on the PC"
                "lock" -> "PC is locked"
                else -> "Secure screen open on the PC"
            }
            val detail = when {
                denyPending -> "Closing the prompt…"
                denyError.isNotEmpty() -> "Couldn't close the prompt ($denyError)."
                "view" !in s.capabilities -> "BubblePlay can't show this screen. The stream resumes when it closes."
                "input" in s.capabilities -> if (s.kind == "lock") "You can sign in with the keyboard." else "You can use it with mouse and keyboard."
                s.kind == "uac" -> "View only. Turn on Admin mode on the PC to approve prompts."
                else -> "View only. Turn on Admin mode on the PC to use it."
            }
            return title to detail
        }

        // BubblePlay dark palette, as in QuickBar/TransferDock.
        private const val DUVET = 0xFF2F3138.toInt()
        private const val THREAD = 0xFF575B64.toInt()
        private const val MILK = 0xFFF3EFE7.toInt()
        private const val CREAM = 0xFFC6BAAC.toInt()
        private const val APRICOT = 0xFFF0A064.toInt()
        private const val INK = 0xFF2F3138.toInt()
    }
}
