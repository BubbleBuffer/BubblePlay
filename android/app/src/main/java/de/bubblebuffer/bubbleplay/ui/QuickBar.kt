package de.bubblebuffer.bubbleplay.ui

import android.annotation.SuppressLint
import android.app.Activity
import android.content.Context
import android.content.res.ColorStateList
import android.graphics.Color
import android.graphics.Typeface
import android.graphics.drawable.ColorDrawable
import android.graphics.drawable.Drawable
import android.graphics.drawable.GradientDrawable
import android.graphics.drawable.StateListDrawable
import android.os.Build
import android.os.Handler
import android.os.Looper
import android.text.TextUtils
import android.util.TypedValue
import android.view.Gravity
import android.view.MotionEvent
import android.view.View
import android.view.ViewGroup
import android.widget.FrameLayout
import android.widget.ImageButton
import android.widget.ImageView
import android.widget.LinearLayout
import android.widget.ScrollView
import android.widget.TextView
import de.bubblebuffer.bubbleplay.R
import de.bubblebuffer.bubbleplay.input.Chord
import de.bubblebuffer.bubbleplay.input.Shortcuts
import de.bubblebuffer.bubbleplay.input.TouchMode
import de.bubblebuffer.bubbleplay.net.Session
import kotlin.math.roundToInt

/**
 * The stream screen's quick-action bar: a flat icon strip docked on the bottom
 * edge (above the soft keyboard when it is open) with in-window popovers
 * (screens, shortcuts, keys, arrows, more). It is always visible and never
 * overlays the video: the stream is laid out in the area above it
 * ([height]).
 *
 * Popovers are views inside the activity window, not PopupWindows: a
 * transparent scrim under them eats the outside tap (it never reaches the
 * remote desktop) and the soft keyboard keeps focus while the Keys popover is
 * open, so sticky modifiers combine with typing.
 */
class QuickBar(private val activity: Activity, private val root: FrameLayout, private val dock: LinearLayout, private val host: Host) {

    interface Host {
        val keyboardShown: Boolean
        val touchMode: TouchMode
        val zoomed: Boolean
        val immersive: Boolean
        val latched: Set<String>
        val monitors: List<Session.Monitor>
        val monitorError: String?
        val switchingMonitor: String?
        /** The host has file transfer v1 (shows Attach). */
        val fileTransfer: Boolean
        val remoteClipboardRow: RemoteClipboard.Row
        fun toggleKeyboard()
        fun toggleTouchMode()
        fun resetZoom()
        fun toggleImmersive()
        fun refreshMonitors()
        fun switchMonitor(selector: String)
        fun sendChord(chord: Chord)
        fun pastePhoneClipboard()
        fun copyPcClipboard()
        fun toggleLatch(code: String)
        /** Tap a key with any latched modifiers (then the latch clears). */
        fun tapKey(code: String)
        /** Hold-to-repeat: first press (takes the latched modifiers), repeats, release. */
        fun keyDown(code: String)
        fun keyRepeat(code: String)
        fun keyUp(code: String)
        fun disconnect()
        fun openSettings()
        fun attachFiles()
    }

    private enum class Kind { SCREEN, SHORTCUTS, KEYS, ARROWS, MORE }

    private val density = activity.resources.displayMetrics.density
    private val timers = Handler(Looper.getMainLooper())

    private val bar: LinearLayout
    private val row: LinearLayout
    private val scrim: View
    private val keyboardItem: ImageButton
    private val screenItem: ImageButton
    private val modeItem: ImageButton
    private val shortcutsItem: ImageButton
    private val keysItem: ImageButton
    private val arrowsItem: ImageButton
    private val attachItem: ImageButton
    private val zoomItem: ImageButton
    private val latchLabel: TextView
    private val moreItem: ImageButton
    private val items: Map<Kind, ImageButton>

    private var panel: View? = null
    private var openKind: Kind? = null
    private val modifierKeys = HashMap<String, TextView>()

    /** The docked bar's height in px (hairline plus one row of 48 dp targets). */
    val height: Int get() = dp(1) + dp(BAR_ITEM_DP)

    private var repeating: String? = null
    private val repeatTick = object : Runnable {
        override fun run() {
            val code = repeating ?: return
            host.keyRepeat(code)
            timers.postDelayed(this, REPEAT_INTERVAL_MS)
        }
    }

    init {
        scrim = View(activity).apply {
            visibility = View.GONE
            importantForAccessibility = View.IMPORTANT_FOR_ACCESSIBILITY_NO
        }
        // The outside tap only closes the popover; the whole gesture is consumed.
        scrim.setOnTouchListener(ScrimTouch())
        root.addView(scrim, FrameLayout.LayoutParams(ViewGroup.LayoutParams.MATCH_PARENT, ViewGroup.LayoutParams.MATCH_PARENT))

        bar = LinearLayout(activity).apply {
            orientation = LinearLayout.VERTICAL
            setBackgroundColor(DUVET)
            isClickable = true // touches on the strip never reach the stream
        }
        bar.addView(View(activity).apply { setBackgroundColor(THREAD) }, LinearLayout.LayoutParams(ViewGroup.LayoutParams.MATCH_PARENT, dp(1)))
        row = FitRow(activity).apply {
            orientation = LinearLayout.HORIZONTAL
            gravity = Gravity.CENTER_VERTICAL
            setPadding(dp(4), 0, dp(4), 0)
        }
        bar.addView(row, LinearLayout.LayoutParams(ViewGroup.LayoutParams.MATCH_PARENT, ViewGroup.LayoutParams.WRAP_CONTENT))

        keyboardItem = item(R.drawable.ic_bar_keyboard, R.string.bar_keyboard_show) { host.toggleKeyboard(); render() }
        screenItem = item(R.drawable.ic_bar_monitor, R.string.bar_screen) { toggle(Kind.SCREEN) }
        modeItem = item(R.drawable.ic_bar_trackpad, R.string.bar_shortcuts) { host.toggleTouchMode(); render() }
        shortcutsItem = item(R.drawable.ic_bar_shortcuts, R.string.bar_shortcuts) { toggle(Kind.SHORTCUTS) }
        keysItem = item(R.drawable.ic_bar_keys, R.string.bar_keys) { toggle(Kind.KEYS) }
        arrowsItem = item(R.drawable.ic_bar_arrows, R.string.bar_arrows) { toggle(Kind.ARROWS) }
        attachItem = item(R.drawable.ic_bar_attach, R.string.bar_attach) { dismiss(); host.attachFiles() }
        zoomItem = item(R.drawable.ic_bar_zoom_reset, R.string.bar_zoom_reset) { host.resetZoom(); render() }
        // Held modifiers, in mono beside the Keys item: the one accent on the bar.
        latchLabel = TextView(activity).apply {
            typeface = Typeface.MONOSPACE
            setTextSize(TypedValue.COMPLEX_UNIT_SP, 12f)
            setTextColor(APRICOT)
            isSingleLine = true
            ellipsize = TextUtils.TruncateAt.END
            setPadding(dp(8), 0, dp(4), 0)
            importantForAccessibility = View.IMPORTANT_FOR_ACCESSIBILITY_NO // announced via the Keys item
        }
        row.addView(latchLabel, LinearLayout.LayoutParams(0, ViewGroup.LayoutParams.WRAP_CONTENT, 1f))
        moreItem = item(R.drawable.ic_bar_more, R.string.bar_more) { toggle(Kind.MORE) }
        items = mapOf(Kind.SCREEN to screenItem, Kind.SHORTCUTS to shortcutsItem, Kind.KEYS to keysItem, Kind.ARROWS to arrowsItem, Kind.MORE to moreItem)

        dock.addView(bar, LinearLayout.LayoutParams(ViewGroup.LayoutParams.MATCH_PARENT, ViewGroup.LayoutParams.WRAP_CONTENT))
        render()
    }

    // ---- bar -----------------------------------------------------------------------

    private fun item(icon: Int, label: Int, onClick: () -> Unit): ImageButton = ImageButton(activity).apply {
        setImageResource(icon)
        imageTintList = ICON_TINT
        scaleType = ImageView.ScaleType.CENTER
        background = controlBackground()
        setPadding(0, 0, 0, 0)
        contentDescription = activity.getString(label)
        tooltipText = contentDescription
        // Hardware keys go to the remote desktop, so the hidden keyboard sink keeps focus.
        isFocusable = false
        setOnClickListener { onClick() }
        row.addView(this, LinearLayout.LayoutParams(dp(BAR_ITEM_DP), dp(BAR_ITEM_DP)))
    }

    /** Re-reads every state from the host. Cheap; call after any change. */
    fun render() {
        val keyboard = host.keyboardShown
        keyboardItem.isActivated = keyboard
        describe(keyboardItem, activity.getString(if (keyboard) R.string.bar_keyboard_hide else R.string.bar_keyboard_show))

        val single = host.monitors.size == 1
        screenItem.isEnabled = !single
        describe(screenItem, activity.getString(if (single) R.string.bar_screen_single else R.string.bar_screen))

        val direct = host.touchMode == TouchMode.DIRECT
        modeItem.setImageResource(if (direct) R.drawable.ic_bar_direct else R.drawable.ic_bar_trackpad)
        describe(modeItem, activity.getString(R.string.bar_mode, activity.getString(if (direct) R.string.mode_direct else R.string.mode_trackpad)))

        zoomItem.visibility = if (host.zoomed) View.VISIBLE else View.GONE
        attachItem.visibility = if (host.fileTransfer) View.VISIBLE else View.GONE

        val latched = host.latched
        val names = Shortcuts.MODIFIERS.filter { it in latched }.joinToString("+") { Shortcuts.keyName(it) }
        latchLabel.text = names
        describe(keysItem, if (names.isEmpty()) activity.getString(R.string.bar_keys) else activity.getString(R.string.bar_keys_latched, names))
        for ((kind, view) in items) view.isActivated = openKind == kind || (kind == Kind.KEYS && latched.isNotEmpty())

        for ((code, key) in modifierKeys) renderModifier(key, code, code in latched)
    }

    private fun describe(view: View, text: String) {
        if (view.contentDescription == text) return
        view.contentDescription = text
        view.tooltipText = text
    }

    /** Activity paused or input lost: let go of a held key and close popovers. */
    fun release() {
        stopRepeat()
        dismiss()
    }

    fun onDestroy() {
        stopRepeat()
        timers.removeCallbacksAndMessages(null)
    }

    // ---- popovers ----------------------------------------------------------------------

    val popoverOpen: Boolean get() = panel != null

    private fun toggle(kind: Kind) {
        if (openKind == kind) { dismiss(); return }
        dismiss()
        if (kind == Kind.SCREEN) host.refreshMonitors()
        open(kind)
    }

    private fun open(kind: Kind) {
        val anchor = items.getValue(kind)
        val content = when (kind) {
            Kind.SCREEN -> screenContent()
            Kind.SHORTCUTS -> shortcutsContent()
            Kind.KEYS -> keysContent()
            Kind.ARROWS -> arrowsContent()
            Kind.MORE -> moreContent()
        }
        val frame = FrameLayout(activity).apply {
            background = GradientDrawable().apply {
                cornerRadii = radii(12f, 18f, 10f, 16f)
                setColor(SURFACE)
                setStroke(dp(1), THREAD)
            }
            setPadding(dp(8), dp(8), dp(8), dp(8))
            isClickable = true
            addView(content)
        }
        openKind = kind
        panel = frame
        scrim.visibility = View.VISIBLE
        root.addView(frame, root.indexOfChild(dock), FrameLayout.LayoutParams(ViewGroup.LayoutParams.WRAP_CONTENT, ViewGroup.LayoutParams.WRAP_CONTENT, Gravity.BOTTOM or Gravity.START))
        position(frame, anchor)
        render()
    }

    /** Above its bar item (and any transfer rows), clamped inside the window with an 8 dp margin. */
    private fun position(frame: View, anchor: View) {
        val width = root.width - root.paddingLeft - root.paddingRight
        val height = root.height - root.paddingTop - root.paddingBottom
        val above = dock.height + dp(6)
        val maxHeight = (height - above - dp(8)).coerceAtLeast(dp(120))
        (frame as ViewGroup).getChildAt(0).let { if (it is CappedScroll) it.maxHeight = maxHeight - dp(16) }
        frame.measure(
            View.MeasureSpec.makeMeasureSpec((width - dp(16)).coerceAtLeast(0), View.MeasureSpec.AT_MOST),
            View.MeasureSpec.makeMeasureSpec(maxHeight, View.MeasureSpec.AT_MOST),
        )
        val anchorAt = IntArray(2).also { anchor.getLocationInWindow(it) }
        val rootAt = IntArray(2).also { root.getLocationInWindow(it) }
        val centre = anchorAt[0] - rootAt[0] - root.paddingLeft + anchor.width / 2
        val left = (centre - frame.measuredWidth / 2).coerceIn(dp(8), (width - frame.measuredWidth - dp(8)).coerceAtLeast(dp(8)))
        val lp = frame.layoutParams as FrameLayout.LayoutParams
        lp.leftMargin = left
        lp.bottomMargin = above
        frame.layoutParams = lp
    }

    /** Closes the open popover; false if none was open (for Back). */
    fun dismiss(): Boolean {
        val frame = panel ?: return false
        stopRepeat()
        root.removeView(frame)
        panel = null
        openKind = null
        modifierKeys.clear()
        scrim.visibility = View.GONE
        render()
        return true
    }

    /** New monitor data (list, error or switch state): refresh the Screens popover if it is open. */
    fun monitorsChanged() {
        render()
        if (openKind == Kind.SCREEN) { dismissQuietly(); open(Kind.SCREEN) }
    }

    /** A PC snapshot arrived or was consumed: refresh the More popover if it is open. */
    fun clipboardChanged() = reopen(Kind.MORE)

    /** Full-screen state changed: refresh the More popover in place. */
    private fun reopen(kind: Kind) { if (openKind == kind) { dismissQuietly(); open(kind) } }

    private fun dismissQuietly() {
        val frame = panel ?: return
        root.removeView(frame)
        panel = null; openKind = null; modifierKeys.clear()
    }

    private fun screenContent(): View {
        val column = column()
        column.addView(caption(R.string.pop_screens))
        val error = host.monitorError
        val monitors = host.monitors
        when {
            error != null -> column.addView(note(activity.getString(R.string.monitor_failed, error), ATTENTION))
            monitors.isEmpty() -> column.addView(note(activity.getString(R.string.pop_screens_loading), CREAM))
            else -> for (m in monitors) {
                val switching = host.switchingMonitor == m.selector && !m.selected
                val state = when {
                    m.selected -> activity.getString(R.string.pop_screen_current)
                    switching -> activity.getString(R.string.pop_screen_switching)
                    else -> null
                }
                val line = listRow(
                    title = m.name,
                    detail = "${m.width}×${m.height}",
                    trailing = state,
                    check = m.selected,
                ) {
                    if (!m.selected) host.switchMonitor(m.selector)
                    dismiss()
                }
                line.contentDescription = listOfNotNull(activity.getString(R.string.pop_screen_item, m.name, m.width, m.height), state).joinToString(", ")
                line.isActivated = m.selected
                column.addView(line)
            }
        }
        return fixedWidth(column, 280)
    }

    private fun shortcutsContent(): View {
        val column = column()
        column.addView(caption(R.string.pop_shortcuts))
        for (chord in Shortcuts.ALL) {
            column.addView(listRow(activity.getString(chord.label), null, chord.keys, false) {
                host.sendChord(chord)
                dismiss()
            })
        }
        column.addView(divider())
        column.addView(listRow(activity.getString(R.string.pop_paste_phone), null, activity.getString(R.string.pop_paste_phone_hint), false) {
            host.pastePhoneClipboard()
            dismiss()
        })
        return CappedScroll(activity).apply {
            isVerticalScrollBarEnabled = true
            addView(column)
            layoutParams = FrameLayout.LayoutParams(dp(280), ViewGroup.LayoutParams.WRAP_CONTENT)
        }
    }

    private fun keysContent(): View {
        val column = column()
        column.addView(caption(R.string.pop_keys))
        column.addView(keyRow(Shortcuts.MODIFIERS.map { code ->
            key(Shortcuts.keyName(code)) { host.toggleLatch(code); render() }.also { modifierKeys[code] = it }
        }))
        column.addView(keyRow(listOf(tapKey("Escape"), tapKey("Tab"), tapKey("Home"), tapKey("End"), holdKey("PageUp"), holdKey("PageDown"))))
        column.addView(keyRow(listOf(holdKey("Delete"), holdKey("ArrowLeft", R.string.key_left), holdKey("ArrowUp", R.string.key_up),
            holdKey("ArrowDown", R.string.key_down), holdKey("ArrowRight", R.string.key_right))))
        column.addView(keyRow((1..6).map { tapKey("F$it") }))
        column.addView(keyRow((7..12).map { tapKey("F$it") }))
        column.addView(note(activity.getString(R.string.pop_keys_hint), CREAM))
        render()
        return CappedScroll(activity).apply {
            addView(column)
            layoutParams = FrameLayout.LayoutParams(dp(312), ViewGroup.LayoutParams.WRAP_CONTENT)
        }
    }

    /** A compact D-pad; arrows and PgUp/PgDn repeat while held. */
    private fun arrowsContent(): View {
        val column = column()
        column.addView(caption(R.string.pop_arrows))
        fun cell(view: TextView) = view.also { it.layoutParams = LinearLayout.LayoutParams(dp(56), dp(52)).apply { setMargins(dp(2), dp(2), dp(2), dp(2)) } }
        fun line(vararg views: TextView) = LinearLayout(activity).apply { orientation = LinearLayout.HORIZONTAL; views.forEach { addView(cell(it)) } }
        column.addView(line(small(tapKey("Home")), holdKey("ArrowUp", R.string.key_up), small(holdKey("PageUp"))))
        column.addView(line(holdKey("ArrowLeft", R.string.key_left), small(tapKey("Enter")), holdKey("ArrowRight", R.string.key_right)))
        column.addView(line(small(tapKey("End")), holdKey("ArrowDown", R.string.key_down), small(holdKey("PageDown"))))
        return column
    }

    private fun moreContent(): View {
        val column = column()
        column.addView(caption(R.string.pop_more))
        val clipboard = host.remoteClipboardRow
        val detail = if (clipboard.enabled) activity.resources.getQuantityString(R.plurals.pop_pc_clipboard_text, clipboard.characters, clipboard.characters)
        else activity.getString(R.string.pop_pc_clipboard_empty)
        column.addView(listRow(activity.getString(R.string.pop_copy_pc_clipboard), detail, null, false) {
            host.copyPcClipboard()
            dismiss()
        }.apply {
            isEnabled = clipboard.enabled
            alpha = if (clipboard.enabled) 1f else 0.55f
        })
        column.addView(listRow(activity.getString(R.string.pop_fullscreen), null,
            activity.getString(if (host.immersive) R.string.pop_on else R.string.pop_off), false) {
            host.toggleImmersive()
            // The window insets change after this; re-anchor on the next layout.
            root.post { reopen(Kind.MORE) }
        })
        column.addView(listRow(activity.getString(R.string.pop_settings), null, null, false) {
            dismiss()
            host.openSettings()
        })
        column.addView(divider())
        column.addView(listRow(activity.getString(R.string.pop_disconnect), null, null, false, DANGER) {
            dismiss()
            host.disconnect()
        })
        return fixedWidth(column, 260)
    }

    // ---- popover building blocks ----------------------------------------------------------

    private fun column() = LinearLayout(activity).apply { orientation = LinearLayout.VERTICAL }

    private fun fixedWidth(view: View, widthDp: Int): View = view.apply {
        layoutParams = FrameLayout.LayoutParams(dp(widthDp), ViewGroup.LayoutParams.WRAP_CONTENT)
    }

    private fun caption(text: Int) = TextView(activity).apply {
        setText(text)
        isAllCaps = true
        typeface = Typeface.MONOSPACE
        letterSpacing = 0.06f
        setTextSize(TypedValue.COMPLEX_UNIT_SP, 11f)
        setTextColor(CREAM)
        setPadding(dp(8), dp(2), dp(8), dp(6))
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.P) isAccessibilityHeading = true
    }

    private fun note(text: String, color: Int) = TextView(activity).apply {
        this.text = text
        setTextSize(TypedValue.COMPLEX_UNIT_SP, 13f)
        setTextColor(color)
        setPadding(dp(8), dp(6), dp(8), dp(4))
    }

    private fun divider() = View(activity).apply {
        setBackgroundColor(THREAD)
        layoutParams = LinearLayout.LayoutParams(ViewGroup.LayoutParams.MATCH_PARENT, dp(1)).apply { setMargins(dp(4), dp(6), dp(4), dp(6)) }
    }

    /** A 48 dp list row: optional check, title (+ detail line), trailing mono text. */
    private fun listRow(title: String, detail: String?, trailing: String?, check: Boolean, titleColor: Int = MILK, onClick: () -> Unit): View {
        val line = LinearLayout(activity).apply {
            orientation = LinearLayout.HORIZONTAL
            gravity = Gravity.CENTER_VERTICAL
            minimumHeight = dp(48)
            setPadding(dp(8), dp(4), dp(8), dp(4))
            background = rowBackground()
            isClickable = true
            isFocusable = false
            setOnClickListener { onClick() }
        }
        if (check) {
            line.addView(ImageView(activity).apply {
                setImageResource(R.drawable.ic_bar_check)
                imageTintList = ColorStateList.valueOf(MILK)
                importantForAccessibility = View.IMPORTANT_FOR_ACCESSIBILITY_NO
            }, LinearLayout.LayoutParams(dp(20), dp(20)).apply { marginEnd = dp(8) })
        }
        val texts = LinearLayout(activity).apply { orientation = LinearLayout.VERTICAL }
        texts.addView(TextView(activity).apply {
            text = title
            setTextSize(TypedValue.COMPLEX_UNIT_SP, 15f)
            setTextColor(titleColor)
        })
        if (detail != null) texts.addView(TextView(activity).apply {
            text = detail
            typeface = Typeface.MONOSPACE
            setTextSize(TypedValue.COMPLEX_UNIT_SP, 12f)
            setTextColor(CREAM)
        })
        line.addView(texts, LinearLayout.LayoutParams(0, ViewGroup.LayoutParams.WRAP_CONTENT, 1f))
        if (trailing != null) line.addView(TextView(activity).apply {
            text = trailing
            typeface = Typeface.MONOSPACE
            setTextSize(TypedValue.COMPLEX_UNIT_SP, 12f)
            setTextColor(CREAM)
            setPadding(dp(8), 0, 0, 0)
        })
        return line
    }

    private fun keyRow(keys: List<TextView>) = LinearLayout(activity).apply {
        orientation = LinearLayout.HORIZONTAL
        for (key in keys) addView(key, LinearLayout.LayoutParams(0, dp(44), 1f).apply { setMargins(dp(2), dp(2), dp(2), dp(2)) })
    }

    private fun key(label: String, onClick: (() -> Unit)? = null) = TextView(activity).apply {
        text = label
        gravity = Gravity.CENTER
        typeface = Typeface.MONOSPACE
        setTextSize(TypedValue.COMPLEX_UNIT_SP, 14f)
        setTextColor(KEY_TEXT)
        background = keyBackground()
        isClickable = true
        isFocusable = false
        if (onClick != null) setOnClickListener { onClick() }
    }

    private fun small(view: TextView) = view.apply { setTextSize(TypedValue.COMPLEX_UNIT_SP, 11f) }

    private fun tapKey(code: String) = key(Shortcuts.keyName(code)) { host.tapKey(code) }

    /** Held keys repeat (Windows does not auto-repeat injected keys); a click (TalkBack) taps once. */
    @SuppressLint("ClickableViewAccessibility")
    private fun holdKey(code: String, description: Int? = null) = key(Shortcuts.keyName(code)) { host.tapKey(code) }.apply {
        if (description != null) contentDescription = activity.getString(description)
        setOnTouchListener { view, event ->
            when (event.actionMasked) {
                MotionEvent.ACTION_DOWN -> { view.isPressed = true; startRepeat(code) }
                MotionEvent.ACTION_UP, MotionEvent.ACTION_CANCEL -> { view.isPressed = false; stopRepeat() }
            }
            true
        }
    }

    private fun renderModifier(key: TextView, code: String, latched: Boolean) {
        key.isActivated = latched
        val name = Shortcuts.keyName(code)
        key.contentDescription = if (latched) activity.getString(R.string.pop_latched, name) else name
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.R) key.stateDescription = if (latched) activity.getString(R.string.pop_on) else activity.getString(R.string.pop_off)
        // Held is also shown by shape (inverted fill), not only by colour.
        key.paintFlags = if (latched) key.paintFlags or android.graphics.Paint.UNDERLINE_TEXT_FLAG else key.paintFlags and android.graphics.Paint.UNDERLINE_TEXT_FLAG.inv()
    }

    private fun startRepeat(code: String) {
        stopRepeat()
        repeating = code
        host.keyDown(code)
        render()
        timers.postDelayed(repeatTick, REPEAT_DELAY_MS)
    }

    private fun stopRepeat() {
        val code = repeating ?: return
        repeating = null
        timers.removeCallbacks(repeatTick)
        host.keyUp(code)
    }

    // ---- drawing ---------------------------------------------------------------------------

    private fun dp(value: Int) = (value * density).roundToInt()
    private fun dpf(value: Float) = value * density

    /** Fitted corners: top-left, top-right, bottom-right, bottom-left (dp). */
    private fun radii(tl: Float, tr: Float, br: Float, bl: Float) =
        floatArrayOf(tl, tl, tr, tr, br, br, bl, bl).map { it * density }.toFloatArray()

    private fun shape(fill: Int, stroke: Int? = null) = GradientDrawable().apply {
        cornerRadii = radii(7f, 11f, 6f, 9f)
        setColor(fill)
        if (stroke != null) setStroke(dp(1), stroke)
    }

    private fun controlBackground(): Drawable = StateListDrawable().apply {
        addState(intArrayOf(android.R.attr.state_pressed), shape(SURFACE_MUTED))
        addState(intArrayOf(android.R.attr.state_activated), shape(SURFACE_MUTED, THREAD))
        addState(intArrayOf(), ColorDrawable(Color.TRANSPARENT))
    }

    private fun rowBackground(): Drawable = StateListDrawable().apply {
        addState(intArrayOf(android.R.attr.state_pressed), shape(SURFACE_MUTED))
        addState(intArrayOf(), ColorDrawable(Color.TRANSPARENT))
    }

    private fun keyBackground(): Drawable = StateListDrawable().apply {
        addState(intArrayOf(android.R.attr.state_activated), shape(MILK))
        addState(intArrayOf(android.R.attr.state_pressed), shape(THREAD))
        addState(intArrayOf(), shape(SURFACE_MUTED))
    }

    private inner class ScrimTouch : View.OnTouchListener {
        @SuppressLint("ClickableViewAccessibility")
        override fun onTouch(v: View, event: MotionEvent): Boolean {
            if (event.actionMasked == MotionEvent.ACTION_DOWN) dismiss()
            return true
        }
    }

    /**
     * The bar row: 48 dp items, shrunk evenly (never below 40 dp) when the
     * visible items would not fit the width, so More is never pushed off.
     */
    private inner class FitRow(context: Context) : LinearLayout(context) {
        override fun onMeasure(widthMeasureSpec: Int, heightMeasureSpec: Int) {
            val available = MeasureSpec.getSize(widthMeasureSpec) - paddingLeft - paddingRight
            val buttons = (0 until childCount).map { getChildAt(it) }.filter { it is ImageButton && it.visibility != View.GONE }
            if (buttons.isNotEmpty() && MeasureSpec.getMode(widthMeasureSpec) != MeasureSpec.UNSPECIFIED) {
                val size = (available / buttons.size).coerceIn(dp(40), dp(BAR_ITEM_DP))
                for (b in buttons) (b.layoutParams as LayoutParams).width = size
            }
            super.onMeasure(widthMeasureSpec, heightMeasureSpec)
        }
    }

    /** A ScrollView that grows with its content up to [maxHeight]. */
    private class CappedScroll(context: Context) : ScrollView(context) {
        var maxHeight = Int.MAX_VALUE
        override fun onMeasure(widthMeasureSpec: Int, heightMeasureSpec: Int) {
            val size = MeasureSpec.getSize(heightMeasureSpec)
            val cap = if (MeasureSpec.getMode(heightMeasureSpec) == MeasureSpec.UNSPECIFIED) maxHeight else minOf(maxHeight, size)
            super.onMeasure(widthMeasureSpec, MeasureSpec.makeMeasureSpec(cap, MeasureSpec.AT_MOST))
        }
    }

    private companion object {
        // BubblePlay dark palette (bubblebuffer-design).
        const val DUVET = 0xFF2F3138.toInt()
        const val SURFACE = 0xFF30333A.toInt()
        const val SURFACE_MUTED = 0xFF3B3E45.toInt()
        const val THREAD = 0xFF575B64.toInt()
        const val THREAD_STRONG = 0xFF999DA6.toInt()
        const val MILK = 0xFFF3EFE7.toInt()
        const val CREAM = 0xFFC6BAAC.toInt()
        const val APRICOT = 0xFFF0A064.toInt()
        const val ATTENTION = 0xFFF0D29C.toInt()
        const val DANGER = 0xFFE5B1AA.toInt()

        val ICON_TINT = ColorStateList(
            arrayOf(intArrayOf(-android.R.attr.state_enabled), intArrayOf()),
            intArrayOf(0x73999DA6, MILK), // disabled: lower contrast, same glyph and name
        )
        val KEY_TEXT = ColorStateList(
            arrayOf(intArrayOf(android.R.attr.state_activated), intArrayOf()),
            intArrayOf(DUVET, MILK),
        )

        const val BAR_ITEM_DP = 48
        const val REPEAT_DELAY_MS = 350L
        const val REPEAT_INTERVAL_MS = 40L
    }
}
