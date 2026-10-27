package de.bubblebuffer.bubbleplay.ui

import android.annotation.SuppressLint
import android.app.Activity
import android.content.ClipData
import android.content.ActivityNotFoundException
import android.content.ClipboardManager
import android.content.Context
import android.content.Intent
import android.content.res.ColorStateList
import android.graphics.Color
import android.graphics.Typeface
import android.graphics.drawable.ColorDrawable
import android.graphics.drawable.Drawable
import android.graphics.drawable.GradientDrawable
import android.graphics.drawable.StateListDrawable
import android.os.Build
import android.os.Handler
import android.net.Uri
import android.os.Looper
import android.util.TypedValue
import android.view.Gravity
import android.view.View
import android.view.ViewGroup
import android.widget.FrameLayout
import android.widget.ImageButton
import android.widget.LinearLayout
import android.widget.RadioButton
import android.widget.RadioGroup
import android.widget.ScrollView
import android.widget.SeekBar
import android.widget.Switch
import android.widget.TextView
import android.widget.Toast
import de.bubblebuffer.bubbleplay.R
import de.bubblebuffer.bubbleplay.input.PointerSpeed
import de.bubblebuffer.bubbleplay.net.ServerConfig
import de.bubblebuffer.bubbleplay.net.Session
import okhttp3.Call
import okhttp3.Callback
import okhttp3.OkHttpClient
import okhttp3.Request
import okhttp3.Response
import org.json.JSONObject
import java.io.IOException
import java.util.Locale
import kotlin.math.roundToInt

/**
 * The settings view (`docs/architecture/session-ui.md`, "Settings view (Android)"):
 * Connection, Touch, Mouse and keyboard, Display, Video, Clipboard, Diagnostics, About. It is a view, not an
 * activity, so opening it from the stream keeps the session (an activity on
 * top would start the 30 s background disconnect). Every change is written to
 * [Prefs] at once and reported through [Host.onSettingChanged] so the host
 * can apply it live.
 */
@SuppressLint("ViewConstructor")
class SettingsView(private val activity: Activity, private val prefs: Prefs, private val host: Host) : FrameLayout(activity) {
    interface Host {
        /** The configured server (null when the saved address does not parse). */
        val settingsConfig: ServerConfig?
        /** True while a stream session runs behind the view. */
        val inSession: Boolean
        /** The host's monitors (empty outside a session or before the list arrives). */
        val settingsMonitors: List<Session.Monitor>
        fun diagnostics(): Session.Diagnostics?
        fun probeRoundTrip()
        fun onSettingChanged(setting: Setting)
        fun closeSettings()
    }

    enum class Setting { TOUCH_MODE, POINTER_SPEED, TAP_TO_CLICK, POINTER_MODE, KEYBOARD_LOCK, BACKGROUND, FULLSCREEN, KEEP_SCREEN_ON, DEFAULT_MONITOR, CODEC, AUTO_COPY_REMOTE_CLIPBOARD }

    private val density = resources.displayMetrics.density
    private val timers = Handler(Looper.getMainLooper())
    private val column = LinearLayout(activity).apply { orientation = LinearLayout.VERTICAL }
    private lateinit var monitorList: LinearLayout
    private val diagValues = LinkedHashMap<Int, TextView>()
    private var probes = 0
    private var sourceURL = SourceLink.DEFAULT_URL
    private var sourceCall: Call? = null
    private val sourceHttp = OkHttpClient.Builder().followRedirects(false).build()

    private val refresh = object : Runnable {
        override fun run() {
            renderDiagnostics()
            if (host.inSession && probes++ % PROBE_EVERY == 0) host.probeRoundTrip()
            timers.postDelayed(this, REFRESH_MS)
        }
    }

    init {
        setBackgroundColor(DUVET)
        isClickable = true // nothing underneath (the stream) sees touches
        isFocusable = true
        val frame = LinearLayout(activity).apply { orientation = LinearLayout.VERTICAL }
        frame.addView(header())
        frame.addView(View(activity).apply { setBackgroundColor(THREAD) }, LinearLayout.LayoutParams(ViewGroup.LayoutParams.MATCH_PARENT, dp(1)))
        val scroll = ScrollView(activity).apply { isFillViewport = true }
        val centred = FrameLayout(activity)
        centred.addView(column, FrameLayout.LayoutParams(ViewGroup.LayoutParams.MATCH_PARENT, ViewGroup.LayoutParams.WRAP_CONTENT, Gravity.CENTER_HORIZONTAL))
        scroll.addView(centred)
        frame.addView(scroll, LinearLayout.LayoutParams(ViewGroup.LayoutParams.MATCH_PARENT, 0, 1f))
        addView(frame, LayoutParams(ViewGroup.LayoutParams.MATCH_PARENT, ViewGroup.LayoutParams.MATCH_PARENT))
        column.setPadding(dp(16), dp(8), dp(16), dp(32))
        build()
    }

    override fun onMeasure(widthMeasureSpec: Int, heightMeasureSpec: Int) {
        // A readable column on tablets and in landscape.
        val width = MeasureSpec.getSize(widthMeasureSpec)
        val lp = column.layoutParams
        val target = if (width > dp(MAX_WIDTH_DP)) dp(MAX_WIDTH_DP) else ViewGroup.LayoutParams.MATCH_PARENT
        if (lp != null && lp.width != target) { lp.width = target; column.layoutParams = lp }
        super.onMeasure(widthMeasureSpec, heightMeasureSpec)
    }

    override fun onAttachedToWindow() {
        super.onAttachedToWindow()
        timers.post(refresh)
        loadSourceLink()
    }

    override fun onDetachedFromWindow() {
        sourceCall?.cancel()
        sourceCall = null
        timers.removeCallbacksAndMessages(null)
        super.onDetachedFromWindow()
    }

    /** The host's monitor list changed. */
    fun monitorsChanged() = renderMonitors()

    // ---- sections ----------------------------------------------------------------------------

    private fun build() {
        section(R.string.settings_connection)
        column.addView(switchRow(R.string.settings_stay_connected, R.string.settings_stay_connected_detail, prefs.stayConnected) {
            prefs.stayConnected = it; host.onSettingChanged(Setting.BACKGROUND)
        })

        section(R.string.settings_touch)
        column.addView(fieldLabel(R.string.settings_touch_mode))
        column.addView(segmented(
            listOf(false to R.string.settings_trackpad, true to R.string.settings_direct),
            prefs.directTouch, R.string.settings_touch_mode,
        ) { prefs.directTouch = it; host.onSettingChanged(Setting.TOUCH_MODE) })
        column.addView(slider())
        column.addView(switchRow(R.string.settings_tap_to_click, R.string.settings_tap_to_click_detail, prefs.tapToClick) {
            prefs.tapToClick = it; host.onSettingChanged(Setting.TAP_TO_CLICK)
        })

        section(R.string.settings_mouse_keyboard)
        column.addView(fieldLabel(R.string.settings_pointer_mode))
        column.addView(segmented(
            listOf(false to R.string.settings_pointer_work, true to R.string.settings_pointer_game),
            prefs.gameMode, R.string.settings_pointer_mode,
        ) { prefs.gameMode = it; host.onSettingChanged(Setting.POINTER_MODE) })
        column.addView(detailText(activity.getString(R.string.settings_pointer_mode_detail)).apply { setPadding(0, dp(6), 0, 0) })
        column.addView(switchRow(R.string.settings_keyboard_lock, R.string.settings_keyboard_lock_detail, prefs.keyboardLock) {
            prefs.keyboardLock = it; host.onSettingChanged(Setting.KEYBOARD_LOCK)
        })

        section(R.string.settings_display)
        column.addView(switchRow(R.string.settings_start_fullscreen, null, prefs.startFullscreen) {
            prefs.startFullscreen = it; host.onSettingChanged(Setting.FULLSCREEN)
        })
        column.addView(switchRow(R.string.settings_keep_screen_on, R.string.settings_keep_screen_on_detail, prefs.keepScreenOn) {
            prefs.keepScreenOn = it; host.onSettingChanged(Setting.KEEP_SCREEN_ON)
        })
        column.addView(fieldLabel(R.string.settings_default_monitor))
        monitorList = LinearLayout(activity).apply { orientation = LinearLayout.VERTICAL }
        column.addView(monitorList)
        renderMonitors()

        section(R.string.settings_video)
        column.addView(fieldLabel(R.string.settings_codec))
        column.addView(segmented(
            listOf(Prefs.CODEC_AUTO to R.string.codec_auto, Prefs.CODEC_HEVC to R.string.codec_hevc, Prefs.CODEC_H264 to R.string.codec_h264),
            prefs.codec, R.string.settings_codec,
        ) { prefs.codec = it; host.onSettingChanged(Setting.CODEC) })
        column.addView(detailText(activity.getString(R.string.settings_codec_detail)).apply { setPadding(0, dp(6), 0, 0) })

        section(R.string.settings_clipboard)
        column.addView(switchRow(R.string.settings_auto_copy_remote_clipboard, R.string.settings_auto_copy_remote_clipboard_detail, prefs.autoCopyRemoteClipboard) {
            prefs.autoCopyRemoteClipboard = it; host.onSettingChanged(Setting.AUTO_COPY_REMOTE_CLIPBOARD)
        })

        section(R.string.settings_diagnostics)
        for (label in DIAG_LABELS) column.addView(valueRow(label))
        column.addView(button(R.string.settings_copy_report, primary = true) { copyReport() },
            LinearLayout.LayoutParams(ViewGroup.LayoutParams.WRAP_CONTENT, dp(48)).apply { topMargin = dp(8) })
        renderDiagnostics()

        section(R.string.settings_about)
        column.addView(valueRow(R.string.settings_app_version).also { diagValues[R.string.settings_app_version]?.text = appVersion() })
        column.addView(button(R.string.settings_source_code, primary = false) {
            val intent = Intent(Intent.ACTION_VIEW, Uri.parse(sourceURL))
            try { activity.startActivity(intent) }
            catch (_: ActivityNotFoundException) { Toast.makeText(activity, R.string.settings_no_browser, Toast.LENGTH_SHORT).show() }
        })
    }

    private fun loadSourceLink() {
        sourceCall?.cancel()
        sourceURL = SourceLink.DEFAULT_URL
        val base = host.settingsConfig?.base ?: return
        val request = Request.Builder().url(base.newBuilder().encodedPath("/__auth/methods").query(null).build()).build()
        val call = sourceHttp.newCall(request)
        sourceCall = call
        call.enqueue(object : Callback {
            override fun onFailure(call: Call, e: IOException) {}
            override fun onResponse(call: Call, response: Response) {
                val url = response.use {
                    if (!it.isSuccessful) return
                    try { SourceLink.normalize(JSONObject(it.body?.string().orEmpty()).optString("sourceURL")) }
                    catch (_: Exception) { SourceLink.DEFAULT_URL }
                }
                timers.post { if (sourceCall === call && isAttachedToWindow) sourceURL = url }
            }
        })
    }

    private fun header() = LinearLayout(activity).apply {
        orientation = LinearLayout.HORIZONTAL
        gravity = Gravity.CENTER_VERTICAL
        minimumHeight = dp(56)
        setPadding(dp(4), 0, dp(16), 0)
        addView(ImageButton(activity).apply {
            setImageResource(R.drawable.ic_bar_close)
            imageTintList = ColorStateList.valueOf(MILK)
            background = pressable()
            contentDescription = activity.getString(R.string.settings_close)
            setOnClickListener { host.closeSettings() }
        }, LinearLayout.LayoutParams(dp(48), dp(48)))
        addView(TextView(activity).apply {
            setText(R.string.settings_title)
            setTextSize(TypedValue.COMPLEX_UNIT_SP, 20f)
            setTextColor(MILK)
            setPadding(dp(8), 0, 0, 0)
            isAccessibilityHeading = true
        })
    }

    // ---- connection --------------------------------------------------------------------------

    // ---- display -----------------------------------------------------------------------------

    private fun renderMonitors() {
        if (!::monitorList.isInitialized) return
        monitorList.removeAllViews()
        val saved = prefs.defaultMonitor
        val monitors = host.settingsMonitors
        monitorList.addView(choiceRow(activity.getString(R.string.settings_monitor_host), activity.getString(R.string.settings_monitor_host_detail), saved.isEmpty()) {
            pickMonitor("", "")
        })
        for (m in monitors) {
            val detail = if (m.width > 0 && m.height > 0) "${m.width}×${m.height}${if (m.selected) " · " + activity.getString(R.string.settings_monitor_showing) else ""}"
            else if (m.selected) activity.getString(R.string.settings_monitor_showing) else null
            monitorList.addView(choiceRow(m.name, detail, m.selector == saved) { pickMonitor(m.selector, m.name) })
        }
        if (saved.isNotEmpty() && monitors.none { it.selector == saved }) {
            // Remembered from an earlier session; this host (or no host yet) does not list it.
            monitorList.addView(choiceRow(prefs.defaultMonitorName,
                activity.getString(if (host.inSession && monitors.isNotEmpty()) R.string.settings_monitor_missing else R.string.settings_monitor_saved),
                true) {})
        }
        if (monitors.isEmpty()) monitorList.addView(detailText(activity.getString(
            if (host.inSession) R.string.settings_monitor_waiting else R.string.settings_monitor_offline)).apply { setPadding(0, dp(4), 0, 0) })
    }

    private fun pickMonitor(selector: String, name: String) {
        if (selector == prefs.defaultMonitor) return
        prefs.setDefaultMonitor(selector, name)
        renderMonitors()
        host.onSettingChanged(Setting.DEFAULT_MONITOR)
    }

    // ---- diagnostics -------------------------------------------------------------------------

    private fun renderDiagnostics() {
        val d = host.diagnostics()
        val none = activity.getString(R.string.settings_diag_none)
        fun set(label: Int, value: String?) { diagValues[label]?.text = value ?: none }
        set(R.string.settings_diag_decoder, d?.decoder)
        set(R.string.settings_diag_resolution, d?.takeIf { it.width > 0 && it.height > 0 }?.let { "${it.width}×${it.height}${it.codec?.let { c -> " · $c" } ?: ""}" })
        set(R.string.settings_diag_fps, d?.fps?.let { String.format(Locale.US, "%.1f", it) })
        set(R.string.settings_diag_bitrate, d?.kbps?.let { formatBitrate(it) })
        set(R.string.settings_diag_rtt, d?.rttMs?.let { "$it ms" })
    }

    private fun formatBitrate(kbps: Double) =
        if (kbps >= 1000) String.format(Locale.US, "%.1f Mbit/s", kbps / 1000) else String.format(Locale.US, "%.0f kbit/s", kbps)

    private fun copyReport() {
        renderDiagnostics()
        val config = host.settingsConfig
        val lines = ArrayList<String>()
        lines += "BubblePlay for Android ${appVersion()}"
        lines += "Device: ${Build.MANUFACTURER} ${Build.MODEL}, Android ${Build.VERSION.RELEASE} (API ${Build.VERSION.SDK_INT})"
        lines += "Server: ${config?.base?.host ?: "invalid"}${if (config?.isLab == true) " (lab run)" else ""}"
        lines += "Session: ${if (host.inSession) "running" else "none"}"
        lines += "Codec setting: ${prefs.codec}"
        lines += "Touch: ${if (prefs.directTouch) "direct" else "trackpad"}, pointer speed ${prefs.pointerSpeed}, tap to click ${if (prefs.tapToClick) "on" else "off"}"
        for (label in DIAG_LABELS) lines += "${activity.getString(label)}: ${diagValues[label]?.text}"
        val manager = activity.getSystemService(Context.CLIPBOARD_SERVICE) as ClipboardManager
        manager.setPrimaryClip(ClipData.newPlainText(activity.getString(R.string.settings_report_label), lines.joinToString("\n")))
        // Android 13+ shows its own clipboard confirmation.
        if (Build.VERSION.SDK_INT < 33) Toast.makeText(activity, R.string.settings_report_copied, Toast.LENGTH_SHORT).show()
    }

    private fun appVersion(): String = try {
        val info = activity.packageManager.getPackageInfo(activity.packageName, 0)
        "${info.versionName} (${info.longVersionCode})"
    } catch (_: Exception) { "?" }

    // ---- building blocks ---------------------------------------------------------------------

    private fun section(title: Int) {
        column.addView(TextView(activity).apply {
            setText(title)
            isAllCaps = true
            typeface = Typeface.MONOSPACE
            letterSpacing = 0.06f
            setTextSize(TypedValue.COMPLEX_UNIT_SP, 12f)
            setTextColor(CREAM)
            setPadding(0, dp(24), 0, dp(8))
            isAccessibilityHeading = true
        })
    }

    private fun fieldLabel(text: Int) = TextView(activity).apply {
        setText(text)
        setTextSize(TypedValue.COMPLEX_UNIT_SP, 15f)
        setTextColor(MILK)
        setPadding(0, dp(8), 0, dp(6))
    }

    private fun detailText(text: String) = TextView(activity).apply {
        this.text = text
        setTextSize(TypedValue.COMPLEX_UNIT_SP, 13f)
        setTextColor(CREAM)
    }

    private fun button(label: Int, primary: Boolean, onClick: () -> Unit) = TextView(activity).apply {
        setText(label)
        gravity = Gravity.CENTER
        setTextSize(TypedValue.COMPLEX_UNIT_SP, 15f)
        setTextColor(if (primary) DUVET else MILK)
        background = StateListDrawable().apply {
            addState(intArrayOf(android.R.attr.state_pressed), shape(if (primary) CREAM else THREAD))
            addState(intArrayOf(), shape(if (primary) MILK else SURFACE_MUTED))
        }
        setPadding(dp(16), 0, dp(16), 0)
        minWidth = dp(64)
        isClickable = true
        isFocusable = true
        setOnClickListener { onClick() }
    }

    private fun choiceRow(title: String, detail: String?, checked: Boolean, onClick: () -> Unit) = LinearLayout(activity).apply {
        orientation = LinearLayout.HORIZONTAL
        gravity = Gravity.CENTER_VERTICAL
        minimumHeight = dp(52)
        setPadding(dp(4), dp(6), dp(4), dp(6))
        background = pressable()
        isClickable = true
        isFocusable = true
        setOnClickListener { onClick() }
        val mark = android.widget.ImageView(activity).apply {
            setImageResource(R.drawable.ic_bar_check)
            imageTintList = ColorStateList.valueOf(MILK)
            visibility = if (checked) View.VISIBLE else View.INVISIBLE
            importantForAccessibility = View.IMPORTANT_FOR_ACCESSIBILITY_NO
        }
        addView(mark, LinearLayout.LayoutParams(dp(20), dp(20)).apply { marginEnd = dp(12) })
        val texts = LinearLayout(activity).apply { orientation = LinearLayout.VERTICAL }
        texts.addView(TextView(activity).apply {
            text = title
            setTextSize(TypedValue.COMPLEX_UNIT_SP, 16f)
            setTextColor(MILK)
        })
        if (detail != null) texts.addView(detailText(detail).apply { typeface = Typeface.MONOSPACE; setTextSize(TypedValue.COMPLEX_UNIT_SP, 12f) })
        addView(texts, LinearLayout.LayoutParams(0, ViewGroup.LayoutParams.WRAP_CONTENT, 1f))
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.R) {
            stateDescription = activity.getString(if (checked) R.string.settings_selected else R.string.settings_not_selected)
        } else contentDescription = if (checked) "$title, ${activity.getString(R.string.settings_selected)}" else title
    }

    @Suppress("UseSwitchCompatOrMaterialCode") // plain platform widgets, no AppCompat
    private fun switchRow(title: Int, detail: Int?, checked: Boolean, onChange: (Boolean) -> Unit): View {
        val wrap = LinearLayout(activity).apply { orientation = LinearLayout.VERTICAL; setPadding(dp(4), dp(4), dp(4), dp(4)) }
        wrap.addView(Switch(activity).apply {
            setText(title)
            isChecked = checked
            minHeight = dp(48)
            setTextSize(TypedValue.COMPLEX_UNIT_SP, 16f)
            setTextColor(MILK)
            thumbTintList = ColorStateList(arrayOf(intArrayOf(android.R.attr.state_checked), intArrayOf()), intArrayOf(DUVET, THREAD_STRONG))
            trackTintList = ColorStateList(arrayOf(intArrayOf(android.R.attr.state_checked), intArrayOf()), intArrayOf(MILK, THREAD))
            setOnCheckedChangeListener { _, value -> onChange(value) }
        })
        if (detail != null) wrap.addView(detailText(activity.getString(detail)).apply { setPadding(0, 0, dp(64), dp(4)) })
        return wrap
    }

    private fun <T> segmented(options: List<Pair<T, Int>>, selected: T, label: Int, onSelect: (T) -> Unit): View {
        val group = RadioGroup(activity).apply {
            orientation = RadioGroup.HORIZONTAL
            contentDescription = activity.getString(label)
        }
        options.forEachIndexed { i, (value, text) ->
            group.addView(RadioButton(activity).apply {
                id = View.generateViewId()
                setText(text)
                buttonDrawable = null
                gravity = Gravity.CENTER
                setTextSize(TypedValue.COMPLEX_UNIT_SP, 15f)
                setTextColor(ColorStateList(arrayOf(intArrayOf(android.R.attr.state_checked), intArrayOf()), intArrayOf(DUVET, MILK)))
                background = StateListDrawable().apply {
                    addState(intArrayOf(android.R.attr.state_checked), segment(MILK, i, options.size))
                    addState(intArrayOf(android.R.attr.state_pressed), segment(THREAD, i, options.size))
                    addState(intArrayOf(), segment(SURFACE_MUTED, i, options.size))
                }
                isChecked = value == selected
                setOnCheckedChangeListener { _, checked -> if (checked) onSelect(value) }
            }, RadioGroup.LayoutParams(0, dp(48), 1f).apply { if (i > 0) marginStart = dp(2) })
        }
        return group
    }

    private fun slider(): View {
        val wrap = LinearLayout(activity).apply { orientation = LinearLayout.VERTICAL; setPadding(0, dp(12), 0, dp(4)) }
        val top = LinearLayout(activity).apply { orientation = LinearLayout.HORIZONTAL; gravity = Gravity.CENTER_VERTICAL }
        top.addView(TextView(activity).apply {
            setText(R.string.settings_pointer_speed)
            setTextSize(TypedValue.COMPLEX_UNIT_SP, 15f)
            setTextColor(MILK)
        }, LinearLayout.LayoutParams(0, ViewGroup.LayoutParams.WRAP_CONTENT, 1f))
        val value = TextView(activity).apply {
            typeface = Typeface.MONOSPACE
            setTextSize(TypedValue.COMPLEX_UNIT_SP, 13f)
            setTextColor(CREAM)
        }
        top.addView(value)
        wrap.addView(top)
        fun label(level: Int) = activity.getString(R.string.settings_pointer_value, level, PointerSpeed.MAX)
        val seek = SeekBar(activity).apply {
            max = PointerSpeed.MAX - PointerSpeed.MIN
            progress = prefs.pointerSpeed - PointerSpeed.MIN
            contentDescription = activity.getString(R.string.settings_pointer_speed)
            progressTintList = ColorStateList.valueOf(MILK)
            thumbTintList = ColorStateList.valueOf(MILK)
            progressBackgroundTintList = ColorStateList.valueOf(THREAD)
            minimumHeight = dp(48)
            setOnSeekBarChangeListener(object : SeekBar.OnSeekBarChangeListener {
                override fun onProgressChanged(bar: SeekBar, progress: Int, fromUser: Boolean) {
                    val level = progress + PointerSpeed.MIN
                    value.text = label(level)
                    if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.R) bar.stateDescription = label(level)
                    if (fromUser) { prefs.pointerSpeed = level; host.onSettingChanged(Setting.POINTER_SPEED) }
                }
                override fun onStartTrackingTouch(bar: SeekBar) {}
                override fun onStopTrackingTouch(bar: SeekBar) {}
            })
        }
        value.text = label(prefs.pointerSpeed)
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.R) seek.stateDescription = label(prefs.pointerSpeed)
        wrap.addView(seek, LinearLayout.LayoutParams(ViewGroup.LayoutParams.MATCH_PARENT, dp(48)))
        val ends = LinearLayout(activity).apply { orientation = LinearLayout.HORIZONTAL }
        ends.addView(detailText(activity.getString(R.string.settings_slower)), LinearLayout.LayoutParams(0, ViewGroup.LayoutParams.WRAP_CONTENT, 1f))
        ends.addView(detailText(activity.getString(R.string.settings_faster)))
        ends.importantForAccessibility = View.IMPORTANT_FOR_ACCESSIBILITY_NO_HIDE_DESCENDANTS
        wrap.addView(ends)
        return wrap
    }

    private fun valueRow(label: Int): View {
        val line = LinearLayout(activity).apply {
            orientation = LinearLayout.HORIZONTAL
            gravity = Gravity.CENTER_VERTICAL
            minimumHeight = dp(36)
            setPadding(dp(4), dp(4), dp(4), dp(4))
        }
        line.addView(TextView(activity).apply {
            setText(label)
            setTextSize(TypedValue.COMPLEX_UNIT_SP, 14f)
            setTextColor(CREAM)
        }, LinearLayout.LayoutParams(dp(120), ViewGroup.LayoutParams.WRAP_CONTENT))
        val value = TextView(activity).apply {
            typeface = Typeface.MONOSPACE
            setTextSize(TypedValue.COMPLEX_UNIT_SP, 13f)
            setTextColor(MILK)
            setTextIsSelectable(false)
        }
        diagValues[label] = value
        line.addView(value, LinearLayout.LayoutParams(0, ViewGroup.LayoutParams.WRAP_CONTENT, 1f))
        return line
    }

    // ---- drawing -----------------------------------------------------------------------------

    private fun dp(value: Int) = (value * density).roundToInt()

    private fun radii(tl: Float, tr: Float, br: Float, bl: Float) =
        floatArrayOf(tl, tl, tr, tr, br, br, bl, bl).map { it * density }.toFloatArray()

    private fun shape(fill: Int) = GradientDrawable().apply {
        cornerRadii = radii(7f, 11f, 6f, 9f)
        setColor(fill)
    }

    /** Segments share seams; only the outer corners are rounded. */
    private fun segment(fill: Int, index: Int, count: Int) = GradientDrawable().apply {
        val first = index == 0
        val last = index == count - 1
        cornerRadii = radii(if (first) 7f else 2f, if (last) 11f else 2f, if (last) 6f else 2f, if (first) 9f else 2f)
        setColor(fill)
    }

    private fun pressable(): Drawable = StateListDrawable().apply {
        addState(intArrayOf(android.R.attr.state_pressed), shape(SURFACE_MUTED))
        addState(intArrayOf(android.R.attr.state_focused), shape(SURFACE))
        addState(intArrayOf(), ColorDrawable(Color.TRANSPARENT))
    }

    private companion object {
        // BubblePlay dark palette (bubblebuffer-design), as in QuickBar.
        const val DUVET = 0xFF2F3138.toInt()
        const val SURFACE = 0xFF30333A.toInt()
        const val SURFACE_MUTED = 0xFF3B3E45.toInt()
        const val THREAD = 0xFF575B64.toInt()
        const val THREAD_STRONG = 0xFF999DA6.toInt()
        const val MILK = 0xFFF3EFE7.toInt()
        const val CREAM = 0xFFC6BAAC.toInt()

        const val MAX_WIDTH_DP = 560
        const val REFRESH_MS = 1_000L
        const val PROBE_EVERY = 5 // a round-trip probe every 5 refreshes

        val DIAG_LABELS = listOf(
            R.string.settings_diag_decoder,
            R.string.settings_diag_resolution,
            R.string.settings_diag_fps,
            R.string.settings_diag_bitrate,
            R.string.settings_diag_rtt,
        )
    }
}
