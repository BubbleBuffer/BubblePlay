package de.bubblebuffer.bubbleplay.ui

import android.content.Context
import android.graphics.drawable.Drawable
import android.graphics.drawable.GradientDrawable
import android.graphics.drawable.StateListDrawable
import android.text.InputType
import android.util.TypedValue
import android.view.Gravity
import android.view.View
import android.view.ViewGroup
import android.widget.EditText
import android.widget.LinearLayout
import android.widget.TextView
import kotlin.math.roundToInt

/** BubblePlay dark palette (bubblebuffer-design), as SettingsView and QuickBar use it. */
internal object KeyPalette {
    val CANVAS = 0xFF10141A.toInt()
    val DUVET = 0xFF2F3138.toInt()
    val SURFACE = 0xFF30333A.toInt()
    val SURFACE_MUTED = 0xFF3B3E45.toInt()
    val THREAD = 0xFF575B64.toInt()
    val THREAD_STRONG = 0xFF999DA6.toInt()
    val MILK = 0xFFF3EFE7.toInt()
    val CREAM = 0xFFC6BAAC.toInt()
    val APRICOT = 0xFFF0A064.toInt()
    val INK = 0xFF282A2F.toInt()
    val DANGER = 0xFFE5B1AA.toInt()
    val CONSTRUCTIVE = 0xFFCAD8CD.toInt()
    val FOCUS = 0xFF7AB7FF.toInt()
}

/**
 * Platform-widget builders for the device-key screens and the start-screen offer. Shapes use the
 * fitted-control and fitted-field corners from the BubblePlay design; focus is an outline separate
 * from the boundary, so keyboard users can see where they are.
 */
internal class KeyViews(private val context: Context) {
    private val density = context.resources.displayMetrics.density

    enum class Style { PRIMARY, SECONDARY, DANGER }

    fun dp(value: Int): Int = (value * density).roundToInt()

    fun title(text: Int) = TextView(context).apply {
        setText(text)
        setTextSize(TypedValue.COMPLEX_UNIT_SP, 24f)
        setTextColor(KeyPalette.MILK)
        setPadding(0, dp(8), 0, dp(8))
        isAccessibilityHeading = true
    }

    fun body(text: CharSequence) = TextView(context).apply {
        this.text = text
        setTextSize(TypedValue.COMPLEX_UNIT_SP, 15f)
        setTextColor(KeyPalette.CREAM)
        setPadding(0, dp(4), 0, dp(4))
    }

    fun body(text: Int): TextView = body(context.getString(text))

    fun fieldLabel(text: Int) = TextView(context).apply {
        setText(text)
        setTextSize(TypedValue.COMPLEX_UNIT_SP, 14f)
        setTextColor(KeyPalette.MILK)
        setPadding(0, dp(12), 0, dp(2))
    }

    /** A status line, hidden until [tell] sets it. Live region so screen readers hear the result. */
    fun status() = TextView(context).apply {
        setTextSize(TypedValue.COMPLEX_UNIT_SP, 14f)
        setPadding(0, dp(8), 0, dp(4))
        visibility = View.GONE
        accessibilityLiveRegion = View.ACCESSIBILITY_LIVE_REGION_POLITE
    }

    fun tell(view: TextView, text: Int, error: Boolean) {
        view.setText(text)
        view.setTextColor(if (error) KeyPalette.DANGER else KeyPalette.CONSTRUCTIVE)
        view.visibility = View.VISIBLE
    }

    fun field(hint: Int) = EditText(context).apply {
        setHint(hint)
        setHintTextColor(KeyPalette.THREAD_STRONG)
        setTextColor(KeyPalette.MILK)
        setTextSize(TypedValue.COMPLEX_UNIT_SP, 16f)
        background = fieldBackground()
        minHeight = dp(46)
        setPadding(dp(12), dp(10), dp(12), dp(10))
        setSingleLine()
        layoutParams = LinearLayout.LayoutParams(
            ViewGroup.LayoutParams.MATCH_PARENT,
            ViewGroup.LayoutParams.WRAP_CONTENT,
        )
    }

    /** A passphrase field: masked, and kept out of autofill (a key passphrase is not a saved password). */
    fun passwordField(hint: Int) = field(hint).apply {
        inputType = InputType.TYPE_CLASS_TEXT or InputType.TYPE_TEXT_VARIATION_PASSWORD
        importantForAutofill = View.IMPORTANT_FOR_AUTOFILL_NO
    }

    /** A full-width button with a fitted silhouette; the action it performs is its label. */
    fun button(text: Int, style: Style, onClick: () -> Unit): TextView = TextView(context).apply {
        setText(text)
        gravity = Gravity.CENTER
        setTextSize(TypedValue.COMPLEX_UNIT_SP, 15f)
        setTextColor(
            when (style) {
                Style.PRIMARY -> KeyPalette.INK
                Style.SECONDARY -> KeyPalette.MILK
                Style.DANGER -> KeyPalette.DANGER
            },
        )
        background = buttonBackground(style)
        minHeight = dp(46)
        setPadding(dp(16), 0, dp(16), 0)
        isClickable = true
        isFocusable = true
        layoutParams = LinearLayout.LayoutParams(
            ViewGroup.LayoutParams.MATCH_PARENT,
            ViewGroup.LayoutParams.WRAP_CONTENT,
        ).apply { topMargin = dp(10) }
        setOnClickListener { onClick() }
    }

    /** A bounded panel for the start-screen offer. */
    fun panel(): Drawable = GradientDrawable().apply {
        cornerRadii = radii(12f, 18f, 10f, 16f)
        setColor(KeyPalette.SURFACE)
        setStroke(dp(1), KeyPalette.THREAD)
    }

    private fun buttonBackground(style: Style): Drawable {
        val fill = if (style == Style.PRIMARY) KeyPalette.APRICOT else KeyPalette.SURFACE
        val pressed = if (style == Style.PRIMARY) KeyPalette.CREAM else KeyPalette.SURFACE_MUTED
        val stroke = when (style) {
            Style.PRIMARY -> KeyPalette.APRICOT
            Style.SECONDARY -> KeyPalette.THREAD
            Style.DANGER -> KeyPalette.DANGER
        }
        return StateListDrawable().apply {
            addState(intArrayOf(android.R.attr.state_pressed), shape(pressed, stroke, 1))
            addState(intArrayOf(android.R.attr.state_focused), shape(fill, KeyPalette.FOCUS, 2))
            addState(intArrayOf(), shape(fill, stroke, 1))
        }
    }

    private fun fieldBackground(): Drawable = StateListDrawable().apply {
        addState(
            intArrayOf(android.R.attr.state_focused),
            GradientDrawable().apply {
                cornerRadii = radii(6f, 10f, 6f, 8f)
                setColor(KeyPalette.SURFACE)
                setStroke(dp(2), KeyPalette.FOCUS)
            },
        )
        addState(
            intArrayOf(),
            GradientDrawable().apply {
                cornerRadii = radii(6f, 10f, 6f, 8f)
                setColor(KeyPalette.SURFACE)
                setStroke(dp(1), KeyPalette.THREAD)
            },
        )
    }

    private fun shape(fill: Int, stroke: Int, strokeDp: Int) = GradientDrawable().apply {
        cornerRadii = radii(7f, 11f, 6f, 9f)
        setColor(fill)
        setStroke(dp(strokeDp), stroke)
    }

    /** CSS-order corners (top-left, top-right, bottom-right, bottom-left) in pixels. */
    private fun radii(tl: Float, tr: Float, br: Float, bl: Float) =
        floatArrayOf(tl, tl, tr, tr, br, br, bl, bl).map { it * density }.toFloatArray()
}
