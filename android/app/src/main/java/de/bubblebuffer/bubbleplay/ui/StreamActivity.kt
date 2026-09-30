package de.bubblebuffer.bubbleplay.ui

import android.annotation.SuppressLint
import android.app.Activity
import android.app.AlertDialog
import android.content.ClipData
import android.content.ClipboardManager
import android.content.Context
import android.content.Intent
import android.graphics.Color
import android.os.Build
import android.os.Bundle
import android.os.Handler
import android.os.Looper
import android.os.SystemClock
import android.view.Choreographer
import android.view.Gravity
import android.view.InputDevice
import android.view.KeyCharacterMap
import android.view.KeyEvent
import android.view.MotionEvent
import android.view.SurfaceHolder
import android.view.SurfaceView
import android.view.View
import android.view.ViewGroup
import android.view.WindowInsets
import android.view.WindowInsetsController
import android.view.WindowManager
import android.view.inputmethod.InputMethodManager
import android.widget.Button
import android.widget.FrameLayout
import android.widget.HorizontalScrollView
import android.widget.LinearLayout
import android.widget.TextView
import android.widget.Toast
import de.bubblebuffer.bubbleplay.R
import de.bubblebuffer.bubbleplay.input.GestureAction
import de.bubblebuffer.bubbleplay.input.RemoteInput
import de.bubblebuffer.bubbleplay.input.TouchGestures
import de.bubblebuffer.bubbleplay.input.TouchMode
import de.bubblebuffer.bubbleplay.input.Viewport
import de.bubblebuffer.bubbleplay.input.WheelAccumulator
import de.bubblebuffer.bubbleplay.net.ServerConfig
import de.bubblebuffer.bubbleplay.net.Session
import de.bubblebuffer.bubbleplay.protocol.CursorState
import de.bubblebuffer.bubbleplay.protocol.InputPackets
import de.bubblebuffer.bubbleplay.protocol.KeyMap
import kotlin.math.roundToInt

/**
 * Full-screen remote desktop: SurfaceView video, cursor overlay, touch
 * (trackpad / direct), IME and hardware keyboard, captured mouse, clipboard,
 * and a small overlay bar.
 */
class StreamActivity : Activity(), Session.Listener, SurfaceHolder.Callback, KeyboardInputView.Sink {
    private val main = Handler(Looper.getMainLooper())
    private lateinit var prefs: Prefs
    private lateinit var config: ServerConfig
    private var session: Session? = null
    private var input: RemoteInput? = null
    private var surface: android.view.Surface? = null

    private lateinit var root: FrameLayout
    private lateinit var stage: FrameLayout
    private lateinit var video: SurfaceView
    private lateinit var cursorView: CursorView
    private lateinit var status: TextView
    private lateinit var bar: HorizontalScrollView
    private lateinit var modeButton: Button
    private lateinit var keyboardView: KeyboardInputView
    private val modifierButtons = HashMap<String, Button>()

    private val viewport = Viewport()
    private val gestures = TouchGestures()
    private val wheel = WheelAccumulator()
    private var videoWidth = 0
    private var videoHeight = 0

    // Remote cursor, normalized 0..1 over the streamed display.
    private var cursorX = 0.5f
    private var cursorY = 0.5f
    private var cursorVisible = false
    private var cursorSynced = false
    private var localCursorUntil = 0L

    private var latched = LinkedHashSet<String>()
    private val activeLatched = HashMap<String, Set<String>>()
    private var keyboardShown = false
    private var lastStatus: Session.Status? = null
    private var monitorDialogWanted = false
    private var lastClipboardFromHost: String? = null
    private var mouseButtons = 0
    private var motionFramePosted = false

    private val backgroundDisconnect = Runnable { stopSession() }
    private val longPress = Runnable { apply(gestures.onLongPress(SystemClock.uptimeMillis())) }
    private val motionFrame = Choreographer.FrameCallback {
        motionFramePosted = false
        input?.flushMotion()
        if (input?.hasPendingMotion() == true) scheduleMotionFlush()
    }

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        prefs = Prefs(this)
        config = prefs.serverConfig() ?: run { finish(); return }
        gestures.mode = if (prefs.directTouch) TouchMode.DIRECT else TouchMode.TRACKPAD
        buildViews()
        enterImmersive()
        startSession()
    }

    // ---- views ---------------------------------------------------------------
    @SuppressLint("ClickableViewAccessibility")
    private fun buildViews() {
        root = FrameLayout(this).apply { setBackgroundColor(Color.BLACK) }
        stage = FrameLayout(this)
        video = SurfaceView(this).apply {
            holder.addCallback(this@StreamActivity)
            pivotX = 0f; pivotY = 0f
        }
        stage.addView(video, FrameLayout.LayoutParams(1, 1))
        cursorView = CursorView(this)
        stage.addView(cursorView, FrameLayout.LayoutParams(ViewGroup.LayoutParams.MATCH_PARENT, ViewGroup.LayoutParams.MATCH_PARENT))
        stage.setOnTouchListener { _, event -> onStageTouch(event) }
        stage.setOnGenericMotionListener { _, event -> onStageGenericMotion(event) }
        stage.setOnCapturedPointerListener { _, event -> onCapturedPointer(event) }
        stage.addOnLayoutChangeListener { _, _, _, _, _, _, _, _, _ -> relayout() }
        root.addView(stage, FrameLayout.LayoutParams(ViewGroup.LayoutParams.MATCH_PARENT, ViewGroup.LayoutParams.MATCH_PARENT))

        keyboardView = KeyboardInputView(this, this)
        root.addView(keyboardView, FrameLayout.LayoutParams(1, 1))

        status = TextView(this).apply {
            setTextColor(Color.WHITE)
            textSize = 12f
            setPadding(dp(10), dp(4), dp(10), dp(4))
            setBackgroundColor(0x99000000.toInt())
            setOnClickListener { onStatusTapped() }
        }
        root.addView(status, FrameLayout.LayoutParams(ViewGroup.LayoutParams.WRAP_CONTENT, ViewGroup.LayoutParams.WRAP_CONTENT, Gravity.TOP or Gravity.START).apply {
            topMargin = dp(8); leftMargin = dp(8)
        })

        val row = LinearLayout(this).apply { orientation = LinearLayout.HORIZONTAL; setPadding(dp(4), dp(2), dp(4), dp(2)) }
        modeButton = barButton(row, modeLabel()) { toggleMode() }
        barButton(row, getString(R.string.key_keyboard)) { toggleKeyboard() }
        barButton(row, "Esc") { tapKey("Escape") }
        barButton(row, "Tab") { tapKey("Tab") }
        modifierButtons["ControlLeft"] = barButton(row, "Ctrl") { toggleLatch("ControlLeft") }
        modifierButtons["AltLeft"] = barButton(row, "Alt") { toggleLatch("AltLeft") }
        modifierButtons["MetaLeft"] = barButton(row, "Win") { toggleLatch("MetaLeft") }
        modifierButtons["ShiftLeft"] = barButton(row, "Shift") { toggleLatch("ShiftLeft") }
        barButton(row, "←") { tapKey("ArrowLeft") }
        barButton(row, "↑") { tapKey("ArrowUp") }
        barButton(row, "↓") { tapKey("ArrowDown") }
        barButton(row, "→") { tapKey("ArrowRight") }
        barButton(row, getString(R.string.key_paste)) { pasteToHost() }
        barButton(row, getString(R.string.key_display)) { requestMonitors() }
        barButton(row, getString(R.string.key_disconnect)) { finish() }
        bar = HorizontalScrollView(this).apply {
            setBackgroundColor(0xB0101418.toInt())
            isHorizontalScrollBarEnabled = false
            addView(row)
        }
        root.addView(bar, FrameLayout.LayoutParams(ViewGroup.LayoutParams.MATCH_PARENT, ViewGroup.LayoutParams.WRAP_CONTENT, Gravity.BOTTOM))

        root.setOnApplyWindowInsetsListener { v, insets ->
            // Edge-to-edge (targetSdk 35): keep the bar above the soft keyboard.
            val ime = if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.R) insets.getInsets(WindowInsets.Type.ime()).bottom else 0
            v.setPadding(0, 0, 0, ime)
            insets
        }
        setContentView(root)
    }

    private fun barButton(row: LinearLayout, label: String, onClick: () -> Unit): Button {
        val button = Button(this, null, android.R.attr.borderlessButtonStyle).apply {
            text = label
            isAllCaps = false
            setTextColor(Color.WHITE)
            minWidth = dp(44); minimumWidth = dp(44)
            setPadding(dp(10), 0, dp(10), 0)
            isFocusable = false
            setOnClickListener { onClick() }
        }
        row.addView(button, LinearLayout.LayoutParams(ViewGroup.LayoutParams.WRAP_CONTENT, dp(44)))
        return button
    }

    private fun enterImmersive() {
        window.addFlags(WindowManager.LayoutParams.FLAG_KEEP_SCREEN_ON)
        window.attributes = window.attributes.apply {
            layoutInDisplayCutoutMode = WindowManager.LayoutParams.LAYOUT_IN_DISPLAY_CUTOUT_MODE_SHORT_EDGES
        }
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.R) {
            // Edge-to-edge is already enforced on API 35; this covers 30..34.
            @Suppress("DEPRECATION")
            window.setDecorFitsSystemWindows(false)
            window.insetsController?.let {
                it.hide(WindowInsets.Type.systemBars())
                it.systemBarsBehavior = WindowInsetsController.BEHAVIOR_SHOW_TRANSIENT_BARS_BY_SWIPE
            }
        } else {
            @Suppress("DEPRECATION")
            window.decorView.systemUiVisibility = View.SYSTEM_UI_FLAG_IMMERSIVE_STICKY or View.SYSTEM_UI_FLAG_FULLSCREEN or
                View.SYSTEM_UI_FLAG_HIDE_NAVIGATION or View.SYSTEM_UI_FLAG_LAYOUT_FULLSCREEN or
                View.SYSTEM_UI_FLAG_LAYOUT_HIDE_NAVIGATION or View.SYSTEM_UI_FLAG_LAYOUT_STABLE
        }
    }

    override fun onWindowFocusChanged(hasFocus: Boolean) {
        super.onWindowFocusChanged(hasFocus)
        if (hasFocus) enterImmersive() else if (stage.hasPointerCapture()) stage.releasePointerCapture()
    }

    // ---- session lifecycle -----------------------------------------------------
    private fun startSession() {
        if (session != null) return
        val s = Session(config, { Prefs.cookieHeader(config) }, Session.chooseCodec(prefs.codec), this)
        session = s
        input = RemoteInput(s)
        surface?.let { s.decoder.setSurface(it) }
        cursorSynced = false
        s.start()
        window.addFlags(WindowManager.LayoutParams.FLAG_KEEP_SCREEN_ON)
    }

    private fun stopSession() {
        input?.releaseAll()
        session?.stop()
        session = null
        input = null
        window.clearFlags(WindowManager.LayoutParams.FLAG_KEEP_SCREEN_ON)
        onStatus(Session.Status(Session.State.CONNECTING, getString(R.string.status_paused)))
    }

    override fun onStart() {
        super.onStart()
        main.removeCallbacks(backgroundDisconnect)
        if (session == null) startSession()
    }

    override fun onPause() {
        super.onPause()
        input?.releaseAll()
        latched.clear(); renderLatches()
    }

    override fun onStop() {
        super.onStop()
        // Keep the session for a short background trip (app switch), then let go.
        main.postDelayed(backgroundDisconnect, BACKGROUND_GRACE_MS)
    }

    override fun onDestroy() {
        main.removeCallbacksAndMessages(null)
        stopSession()
        super.onDestroy()
    }

    @Deprecated("Deprecated in Java")
    override fun onBackPressed() {
        when {
            keyboardShown -> toggleKeyboard()
            bar.visibility != View.VISIBLE -> bar.visibility = View.VISIBLE
            else -> AlertDialog.Builder(this)
                .setMessage(R.string.confirm_disconnect)
                .setPositiveButton(R.string.key_disconnect) { _, _ -> finish() }
                .setNegativeButton(android.R.string.cancel, null)
                .show()
        }
    }

    // ---- SurfaceHolder -----------------------------------------------------------
    override fun surfaceCreated(holder: SurfaceHolder) {
        surface = holder.surface
        session?.decoder?.setSurface(holder.surface)
    }

    override fun surfaceChanged(holder: SurfaceHolder, format: Int, width: Int, height: Int) {}

    override fun surfaceDestroyed(holder: SurfaceHolder) {
        surface = null
        session?.decoder?.detachSurfaceBlocking()
    }

    // ---- Session.Listener (main thread) --------------------------------------------
    override fun onStatus(status: Session.Status) {
        lastStatus = status
        this.status.text = status.text
        this.status.setTextColor(
            when (status.state) {
                Session.State.CONNECTED, Session.State.STATIC -> 0xFFB8F5C8.toInt()
                Session.State.ERROR -> 0xFFFFB4A8.toInt()
                else -> 0xFFFFE08A.toInt()
            },
        )
    }

    override fun onAuthRequired() {
        stopSession()
        startActivity(Intent(this, LoginActivity::class.java).putExtra(LoginActivity.EXTRA_FORCE, true))
        finish()
    }

    override fun onCursor(state: CursorState) {
        cursorVisible = state.visible
        if (!state.inBounds) { cursorView.update(0f, 0f, false); return }
        // Local prediction wins while a finger drives the cursor; the host's
        // position is authoritative otherwise.
        if (gestures.active || SystemClock.uptimeMillis() < localCursorUntil) return
        cursorX = state.x / 65535f
        cursorY = state.y / 65535f
        cursorSynced = true
        renderCursor()
    }

    override fun onRemoteClipboard(text: String) {
        lastClipboardFromHost = text
        val manager = getSystemService(Context.CLIPBOARD_SERVICE) as ClipboardManager
        manager.setPrimaryClip(ClipData.newPlainText("BubblePlay", text))
    }

    override fun onMonitors(monitors: List<Session.Monitor>, error: String?) {
        if (error != null) { Toast.makeText(this, getString(R.string.monitor_failed, error), Toast.LENGTH_LONG).show(); return }
        if (!monitorDialogWanted || monitors.isEmpty()) return
        monitorDialogWanted = false
        val labels = monitors.map { m -> "${m.name} (${m.width}×${m.height})${if (m.selected) " ✓" else ""}" }.toTypedArray()
        AlertDialog.Builder(this)
            .setTitle(R.string.key_display)
            .setItems(labels) { _, which -> session?.requestMonitor(monitors[which].selector) }
            .show()
    }

    override fun onVideoSize(width: Int, height: Int) {
        videoWidth = width; videoHeight = height
        video.holder.setFixedSize(width, height)
        relayout()
    }

    override fun onInputReady() {
        input?.cursorSync()
    }

    override fun onSecureDesktop(secure: Boolean) {
        if (secure) Toast.makeText(this, R.string.secure_desktop, Toast.LENGTH_SHORT).show()
    }

    // ---- layout ---------------------------------------------------------------------
    private fun relayout() {
        viewport.layout(stage.width, stage.height, videoWidth, videoHeight)
        val lp = video.layoutParams as FrameLayout.LayoutParams
        val w = viewport.contentW.roundToInt()
        val h = viewport.contentH.roundToInt()
        if (lp.width != w || lp.height != h || lp.leftMargin != viewport.contentX.roundToInt() || lp.topMargin != viewport.contentY.roundToInt()) {
            lp.width = w; lp.height = h
            lp.leftMargin = viewport.contentX.roundToInt(); lp.topMargin = viewport.contentY.roundToInt()
            video.layoutParams = lp
        }
        applyViewport()
    }

    private fun applyViewport() {
        val z = viewport.zoom
        video.scaleX = z; video.scaleY = z
        video.translationX = viewport.x + viewport.contentX * (z - 1f)
        video.translationY = viewport.y + viewport.contentY * (z - 1f)
        renderCursor()
    }

    private fun renderCursor() {
        val (sx, sy) = viewport.toScreen(cursorX, cursorY)
        cursorView.update(sx, sy, (cursorVisible || gestures.mode == TouchMode.TRACKPAD) && session != null)
    }

    // ---- touch ------------------------------------------------------------------------
    private fun onStageTouch(event: MotionEvent): Boolean {
        if (event.isFromSource(InputDevice.SOURCE_MOUSE)) return onMouseEvent(event)
        if (stage.hasPointerCapture()) stage.releasePointerCapture()
        val now = event.eventTime
        val actions = ArrayList<GestureAction>()
        when (event.actionMasked) {
            MotionEvent.ACTION_DOWN, MotionEvent.ACTION_POINTER_DOWN -> {
                val i = event.actionIndex
                actions += gestures.down(event.getPointerId(i), event.getX(i), event.getY(i), now)
            }
            MotionEvent.ACTION_MOVE -> for (i in 0 until event.pointerCount) {
                actions += gestures.move(event.getPointerId(i), event.getX(i), event.getY(i))
            }
            MotionEvent.ACTION_UP, MotionEvent.ACTION_POINTER_UP -> actions += gestures.up(event.getPointerId(event.actionIndex), now)
            MotionEvent.ACTION_CANCEL -> actions += gestures.cancel()
        }
        main.removeCallbacks(longPress)
        gestures.longPressDeadline?.let { main.postAtTime(longPress, it) }
        apply(actions)
        return true
    }

    private fun apply(actions: List<GestureAction>) {
        val remote = input ?: return
        for (action in actions) when (action) {
            is GestureAction.Move -> {
                val (dx, dy) = viewport.deltaToNormalized(action.dx * TRACKPAD_GAIN, action.dy * TRACKPAD_GAIN)
                cursorX = (cursorX + dx).coerceIn(0f, 1f); cursorY = (cursorY + dy).coerceIn(0f, 1f)
                localCursorUntil = SystemClock.uptimeMillis() + 300
                remote.absoluteNormalized(cursorX, cursorY)
                viewport.keepVisible(cursorX, cursorY)
                applyViewport()
                scheduleMotionFlush()
            }
            is GestureAction.PointAt -> {
                val (nx, ny) = viewport.toNormalized(action.x, action.y)
                cursorX = nx.coerceIn(0f, 1f); cursorY = ny.coerceIn(0f, 1f)
                localCursorUntil = SystemClock.uptimeMillis() + 300
                remote.absoluteNormalized(cursorX, cursorY)
                renderCursor()
                scheduleMotionFlush()
            }
            is GestureAction.Press -> buttonsAtCursor(remote, remote.buttonMask or action.button)
            is GestureAction.Release -> buttonsAtCursor(remote, remote.buttonMask and action.button.inv())
            is GestureAction.Click -> {
                buttonsAtCursor(remote, action.button)
                buttonsAtCursor(remote, 0)
                stage.performClick()
            }
            is GestureAction.Scroll -> {
                val perNotch = SCROLL_DP_PER_NOTCH * resources.displayMetrics.density
                // Natural scrolling: content follows the fingers.
                val (wx, wy) = wheel.add(-action.dx * 120f / perNotch, action.dy * 120f / perNotch)
                if (wx != 0 || wy != 0) { remote.motion(0, 0, wx, wy); scheduleMotionFlush() }
            }
            is GestureAction.Zoom -> { viewport.zoomBy(action.scale, action.focusX, action.focusY); applyViewport() }
            is GestureAction.Pan -> { viewport.pan(action.dx, action.dy); applyViewport() }
        }
    }

    private fun buttonsAtCursor(remote: RemoteInput, mask: Int) {
        remote.flushMotion()
        remote.buttonsAt((cursorX * 65535f).roundToInt(), (cursorY * 65535f).roundToInt(), mask)
    }

    private fun scheduleMotionFlush() {
        if (motionFramePosted) return
        motionFramePosted = true
        Choreographer.getInstance().postFrameCallback(motionFrame)
    }

    // ---- mouse ------------------------------------------------------------------------
    private fun onStageGenericMotion(event: MotionEvent): Boolean {
        if (!event.isFromSource(InputDevice.SOURCE_MOUSE)) return false
        return onMouseEvent(event)
    }

    /** An uncaptured mouse asks for pointer capture so motion arrives relative. */
    private fun onMouseEvent(event: MotionEvent): Boolean {
        if (!stage.hasPointerCapture() && hasWindowFocus()) stage.requestPointerCapture()
        return true
    }

    private fun onCapturedPointer(event: MotionEvent): Boolean {
        val remote = input ?: return true
        when (event.actionMasked) {
            MotionEvent.ACTION_MOVE, MotionEvent.ACTION_HOVER_MOVE -> {
                var dx = event.x
                var dy = event.y
                for (h in 0 until event.historySize) { dx += event.getHistoricalX(h); dy += event.getHistoricalY(h) }
                remote.motion(dx.roundToInt(), dy.roundToInt())
                scheduleMotionFlush()
            }
            MotionEvent.ACTION_SCROLL -> {
                val (wx, wy) = wheel.add(event.getAxisValue(MotionEvent.AXIS_HSCROLL) * 120f, event.getAxisValue(MotionEvent.AXIS_VSCROLL) * 120f)
                if (wx != 0 || wy != 0) { remote.motion(0, 0, wx, wy); scheduleMotionFlush() }
            }
        }
        val mask = mouseMask(event.buttonState)
        if (mask != mouseButtons) {
            mouseButtons = mask
            remote.flushMotion()
            remote.buttons(mask)
        }
        return true
    }

    private fun mouseMask(state: Int): Int {
        var mask = 0
        if (state and MotionEvent.BUTTON_PRIMARY != 0) mask = mask or InputPackets.BUTTON_LEFT
        if (state and MotionEvent.BUTTON_SECONDARY != 0) mask = mask or InputPackets.BUTTON_RIGHT
        if (state and MotionEvent.BUTTON_TERTIARY != 0) mask = mask or InputPackets.BUTTON_MIDDLE
        if (state and MotionEvent.BUTTON_BACK != 0) mask = mask or InputPackets.BUTTON_X1
        if (state and MotionEvent.BUTTON_FORWARD != 0) mask = mask or InputPackets.BUTTON_X2
        return mask
    }

    // ---- keyboard ------------------------------------------------------------------------
    override fun dispatchKeyEvent(event: KeyEvent): Boolean {
        val device = event.device
        val hardware = device != null && !device.isVirtual && event.deviceId != KeyCharacterMap.VIRTUAL_KEYBOARD
        if (hardware && event.isFromSource(InputDevice.SOURCE_KEYBOARD) && KeyMap.ANDROID_TO_CODE.containsKey(event.keyCode)) {
            return sendKey(event)
        }
        return super.dispatchKeyEvent(event)
    }

    /** Soft-keyboard key events (Enter, Backspace, some IMEs send every key). */
    override fun onImeKey(event: KeyEvent): Boolean {
        val code = KeyMap.ANDROID_TO_CODE[event.keyCode]
        val unicode = event.getUnicodeChar(event.metaState)
        val printable = unicode > 0 && !Character.isISOControl(unicode) && code != "Space"
        if (printable && latched.isEmpty() && !event.isCtrlPressed && !event.isAltPressed) {
            // A character from the IME: send the character, not a US-layout key.
            if (event.action == KeyEvent.ACTION_DOWN) input?.text(String(Character.toChars(unicode)))
            return true
        }
        if (code == null) return false
        return sendKey(event)
    }

    @Suppress("DEPRECATION") // ACTION_MULTIPLE still arrives from some IMEs and HID stacks.
    private fun sendKey(event: KeyEvent): Boolean {
        val code = KeyMap.ANDROID_TO_CODE[event.keyCode] ?: return false
        val remote = input ?: return true
        when (event.action) {
            KeyEvent.ACTION_DOWN -> {
                if (code !in KeyMap.MODIFIERS && latched.isNotEmpty() && event.repeatCount == 0) {
                    val mods = latched.toSet()
                    activeLatched[code] = mods
                    mods.forEach { remote.key(it, false) }
                    latched.clear(); renderLatches()
                }
                remote.key(code, false)
            }
            KeyEvent.ACTION_UP -> {
                remote.key(code, true)
                activeLatched.remove(code)?.forEach { remote.key(it, true) }
            }
            KeyEvent.ACTION_MULTIPLE -> event.characters?.let { remote.text(it) }
        }
        return true
    }

    override fun onImeEdit(deleteCount: Int, insert: String) {
        val remote = input ?: return
        repeat(deleteCount) { remote.tap("Backspace") }
        if (insert.isEmpty()) return
        if (latched.isNotEmpty() && insert.length == 1) {
            val code = KeyMap.codeForShortcutChar(insert[0])
            if (code != null) { tapKey(code); return }
        }
        // Newlines and tabs are keys on the host; everything else is Unicode text.
        val pending = StringBuilder()
        fun flush() { if (pending.isNotEmpty()) { remote.text(pending.toString()); pending.setLength(0) } }
        for (ch in insert) when (ch) {
            '\n' -> { flush(); remote.tap("Enter") }
            '\t' -> { flush(); remote.tap("Tab") }
            else -> pending.append(ch)
        }
        flush()
    }

    override fun onImeDeleteForward(count: Int) {
        val remote = input ?: return
        repeat(count) { remote.tap("Delete") }
    }

    /** Overlay key: tap with any latched modifiers, then clear the latch. */
    private fun tapKey(code: String) {
        val remote = input ?: return
        val mods = latched.toList()
        mods.forEach { remote.key(it, false) }
        remote.tap(code)
        mods.asReversed().forEach { remote.key(it, true) }
        latched.clear(); renderLatches()
    }

    /** Ctrl/Alt/Win/Shift latch for the next key; tapping a latched one sends it alone (Win opens Start). */
    private fun toggleLatch(code: String) {
        if (code in latched) {
            latched.remove(code)
            val others = latched.toList()
            if (others.isEmpty()) input?.tap(code) else tapKeyWith(code, others)
            latched.clear()
        } else latched.add(code)
        renderLatches()
    }

    private fun tapKeyWith(code: String, mods: List<String>) {
        val remote = input ?: return
        mods.forEach { remote.key(it, false) }
        remote.tap(code)
        mods.asReversed().forEach { remote.key(it, true) }
    }

    private fun renderLatches() {
        for ((code, button) in modifierButtons) button.setBackgroundColor(if (code in latched) 0xFF2F6FDB.toInt() else Color.TRANSPARENT)
    }

    private fun toggleKeyboard() {
        val imm = getSystemService(Context.INPUT_METHOD_SERVICE) as InputMethodManager
        keyboardShown = !keyboardShown
        if (keyboardShown) {
            keyboardView.requestFocus()
            imm.restartInput(keyboardView)
            imm.showSoftInput(keyboardView, InputMethodManager.SHOW_IMPLICIT)
        } else {
            imm.hideSoftInputFromWindow(keyboardView.windowToken, 0)
            keyboardView.clearFocus()
        }
    }

    // ---- bar actions ------------------------------------------------------------------------
    private fun modeLabel() = getString(if (gestures.mode == TouchMode.TRACKPAD) R.string.mode_trackpad else R.string.mode_direct)

    private fun toggleMode() {
        gestures.cancel().let { apply(it) }
        gestures.mode = if (gestures.mode == TouchMode.TRACKPAD) TouchMode.DIRECT else TouchMode.TRACKPAD
        prefs.directTouch = gestures.mode == TouchMode.DIRECT
        modeButton.text = modeLabel()
        renderCursor()
    }

    private fun pasteToHost() {
        val manager = getSystemService(Context.CLIPBOARD_SERVICE) as ClipboardManager
        val text = manager.primaryClip?.takeIf { it.itemCount > 0 }?.getItemAt(0)?.coerceToText(this)?.toString()
        if (text.isNullOrEmpty()) { Toast.makeText(this, R.string.clipboard_empty, Toast.LENGTH_SHORT).show(); return }
        val remote = input ?: return
        val ok = if (session?.clipboardEnabled() == true) remote.clipboard(text, paste = true) else remote.text(text)
        if (!ok) Toast.makeText(this, R.string.paste_failed, Toast.LENGTH_SHORT).show()
    }

    private fun requestMonitors() {
        monitorDialogWanted = true
        session?.requestMonitorList()
    }

    private fun onStatusTapped() {
        if (lastStatus?.state == Session.State.ERROR || session == null) {
            stopSession(); startSession(); return
        }
        bar.visibility = if (bar.visibility == View.VISIBLE) View.GONE else View.VISIBLE
    }

    private fun dp(value: Int) = (value * resources.displayMetrics.density).roundToInt()

    companion object {
        private const val BACKGROUND_GRACE_MS = 30_000L
        private const val TRACKPAD_GAIN = 1.6f
        private const val SCROLL_DP_PER_NOTCH = 40f
    }
}
