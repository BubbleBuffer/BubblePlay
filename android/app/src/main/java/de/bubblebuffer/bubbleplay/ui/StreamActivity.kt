package de.bubblebuffer.bubbleplay.ui

import android.Manifest
import android.annotation.SuppressLint
import android.content.ActivityNotFoundException
import android.content.ContentResolver
import android.net.Uri
import android.widget.LinearLayout
import de.bubblebuffer.bubbleplay.files.DownloadNotifier
import de.bubblebuffer.bubbleplay.files.FileTransfers
import de.bubblebuffer.bubbleplay.files.ReceiveCheck
import de.bubblebuffer.bubbleplay.protocol.FileWire
import android.app.Activity
import android.app.AlertDialog
import android.content.ClipData
import android.content.ClipboardManager
import android.content.Context
import android.content.Intent
import android.graphics.Bitmap
import android.graphics.Color
import android.graphics.PorterDuff
import android.os.Build
import android.os.Bundle
import android.os.Handler
import android.os.Looper
import android.os.SystemClock
import android.util.Log
import android.view.Choreographer
import android.view.PixelCopy
import android.view.Gravity
import android.view.HapticFeedbackConstants
import android.view.InputDevice
import android.view.KeyCharacterMap
import android.view.KeyEvent
import android.view.MotionEvent
import android.view.PointerIcon
import android.view.SurfaceHolder
import android.view.SurfaceView
import android.view.View
import android.view.ViewGroup
import android.view.WindowInsets
import android.view.WindowInsetsController
import android.view.WindowManager
import android.view.inputmethod.InputMethodManager
import android.widget.FrameLayout
import android.widget.ImageView
import android.widget.TextView
import android.widget.Toast
import de.bubblebuffer.bubbleplay.R
import de.bubblebuffer.bubbleplay.input.CaptureClick
import de.bubblebuffer.bubbleplay.input.Chord
import de.bubblebuffer.bubbleplay.input.CursorReconciler
import de.bubblebuffer.bubbleplay.input.GestureAction
import de.bubblebuffer.bubbleplay.input.MetaSync
import de.bubblebuffer.bubbleplay.input.PointerSpeed
import de.bubblebuffer.bubbleplay.input.PointerWheel
import de.bubblebuffer.bubbleplay.input.RemoteInput
import de.bubblebuffer.bubbleplay.input.TouchGestures
import de.bubblebuffer.bubbleplay.input.TouchMode
import de.bubblebuffer.bubbleplay.input.Viewport
import de.bubblebuffer.bubbleplay.input.WheelAccumulator
import de.bubblebuffer.bubbleplay.net.SecureDesktop
import de.bubblebuffer.bubbleplay.net.ServerConfig
import de.bubblebuffer.bubbleplay.net.Session
import de.bubblebuffer.bubbleplay.protocol.CursorState
import de.bubblebuffer.bubbleplay.protocol.CursorKind
import de.bubblebuffer.bubbleplay.protocol.InputPackets
import de.bubblebuffer.bubbleplay.protocol.KeyMap
import kotlin.math.roundToInt

/**
 * Full-screen remote desktop: SurfaceView video, cursor overlay, touch
 * (trackpad / direct), IME and hardware keyboard, captured mouse, clipboard,
 * and the quick-action bar ([QuickBar]).
 */
class StreamActivity : Activity(), Session.Listener, SurfaceHolder.Callback, KeyboardInputView.Sink, QuickBar.Host, TransferDock.Host, SettingsView.Host, KeepAlive.Owner {
    private val main = Handler(Looper.getMainLooper())
    private lateinit var prefs: Prefs
    private lateinit var config: ServerConfig
    private var session: Session? = null
    private var input: RemoteInput? = null
    private var surface: android.view.Surface? = null

    private lateinit var root: FrameLayout
    private lateinit var stage: FrameLayout
    private lateinit var video: SurfaceView
    /** The last frame over the stream after a background trip, until a live frame is presented. */
    private val frozen = FrozenFrame<Bitmap>()
    private lateinit var frozenLayer: FrameLayout
    private lateinit var frozenImage: ImageView
    private lateinit var frozenLabel: TextView
    /** When the activity last came to the foreground (0: logged already); for the resume timing log. */
    private var foregroundAt = 0L
    private lateinit var cursorView: CursorView
    private lateinit var status: TextView
    private lateinit var secureBanner: SecureBanner
    private lateinit var keyboardView: KeyboardInputView
    private lateinit var quickBar: QuickBar
    private lateinit var dock: LinearLayout
    private lateinit var transferDock: TransferDock
    /** The settings view, over everything while open; the session keeps running behind it. */
    private var settingsView: SettingsView? = null
    private var pointerGain = PointerSpeed.DEFAULT_GAIN
    /** The default monitor is requested once per session, on the first monitor list. */
    private var defaultMonitorChecked = false

    // File transfer v1: capability from host-capabilities (null = no host yet).
    private var files: FileTransfers? = null
    private var fileCapability: Boolean? = null
    @Volatile private var fileEpoch: String? = null
    private var currentOffer: FileWire.Offer? = null
    /** Uploads asked for before the host said it can take files (share sheet, picker after a reconnect). */
    private val pendingUploads = ArrayList<Pair<List<Uri>, String>>()
    private var askedNotifications = false

    private val viewport = Viewport()
    private val gestures = TouchGestures()
    private val wheel = WheelAccumulator()
    private var videoWidth = 0
    private var videoHeight = 0
    private var barZoomed = false

    // Remote cursor, normalized 0..1 over the streamed display.
    private var cursorX = 0.5f
    private var cursorY = 0.5f
    private var cursorVisible = false
    private val cursorReconciler = CursorReconciler()

    private val latchSet = LinkedHashSet<String>()
    private val activeLatched = HashMap<String, Set<String>>()
    private val latchedKeySender = LatchedKeySender(latchSet, activeLatched,
        { code, up -> input?.key(code, up) == true }, { latchesChanged() })
    private var lastStatus: Session.Status? = null
    private val remoteClipboard = RemoteClipboard()
    private val clipboardManager by lazy { getSystemService(Context.CLIPBOARD_SERVICE) as ClipboardManager }
    private var resumed = false
    private val clipboardChanged = ClipboardManager.OnPrimaryClipChangedListener { observeLocalClipboard() }
    private var mouseButtons = 0
    private var mouseActive = false
    private var motionFramePosted = false
    /** Physical mouse pointer mode: game captures it for relative motion, work points absolutely. */
    private var gameMode = false
    private val captureClick = CaptureClick()
    /** Keyboard lock on: holding Esc for [ESC_HOLD_MS] releases a captured mouse (the Esc itself goes to the PC). */
    private val escapeRelease = Runnable { releaseMouseCapture() }
    /** Keyboard lock off: the Esc that released the mouse; its key-up stays local too. */
    private var escapeSwallowed = false
    private val metaSync = MetaSync()

    // QuickBar.Host state.
    override var keyboardShown = false
        private set
    override var immersive = true
        private set
    override var monitors: List<Session.Monitor> = emptyList()
        private set
    override var monitorError: String? = null
        private set
    override var switchingMonitor: String? = null
        private set
    override val touchMode: TouchMode get() = gestures.mode
    override val zoomed: Boolean get() = viewport.zoomed
    override val latched: Set<String> get() = latchSet
    override val fileTransfer: Boolean get() = fileCapability == true
    override val remoteClipboardRow: RemoteClipboard.Row get() = remoteClipboard.row
    // Follow active window: shown only for a capable host; the host's reported state wins over the saved choice.
    private var followCapable = false
    private var hostFollow: Boolean? = null
    override val followWindow: Boolean? get() = if (followCapable) hostFollow ?: prefs.followWindow else null

    private val backgroundDisconnect = Runnable { stopSession() }
    /** Between onStop and onStart: the app is in the background. */
    private var stopped = false
    private val longPress = Runnable { apply(gestures.onLongPress(SystemClock.uptimeMillis())) }
    private val monitorRefresh = Runnable { session?.requestMonitorList() }
    private val motionFrame = Choreographer.FrameCallback {
        motionFramePosted = false
        input?.flushMotion()
        if (input?.hasPendingInput() == true) scheduleMotionFlush()
    }

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        prefs = Prefs(this)
        remoteClipboard.automatic = prefs.autoCopyRemoteClipboard
        config = prefs.serverConfig() ?: run { finish(); return }
        gestures.mode = if (prefs.directTouch) TouchMode.DIRECT else TouchMode.TRACKPAD
        gestures.tapToClick = prefs.tapToClick
        pointerGain = PointerSpeed.gain(prefs.pointerSpeed)
        gameMode = prefs.gameMode
        immersive = prefs.startFullscreen
        buildViews()
        KeepAlive.owner = this
        clipboardManager.addPrimaryClipChangedListener(clipboardChanged)
        files = FileTransfers(this, { fileEpoch?.let { config.fileUrl(it) } }, { if (config.isLab) null else Prefs.cookieHeader(config) }, fileListener)
        enterImmersive() // also sets up edge-to-edge and the cutout mode
        if (!immersive) exitImmersive()
        startSession()
        takeShares(intent)
    }

    override fun onNewIntent(intent: Intent) {
        super.onNewIntent(intent)
        setIntent(intent)
        takeShares(intent)
    }

    /** Files shared to BubblePlay from another app. */
    private fun takeShares(intent: Intent?) {
        val uris = PendingShares.read(intent)
        PendingShares.consume(intent)
        if (uris.isNotEmpty()) enqueueUpload(uris, FileWire.PURPOSE_SAVE)
    }

    // ---- views ---------------------------------------------------------------
    @SuppressLint("ClickableViewAccessibility")
    private fun buildViews() {
        root = FrameLayout(this).apply { setBackgroundColor(Color.BLACK) }
        stage = object : FrameLayout(this) {
            // ViewGroup normally asks its topmost child first. The full-size
            // cursor overlay must not replace the stream's semantic mouse icon.
            override fun onResolvePointerIcon(event: MotionEvent, pointerIndex: Int): PointerIcon? = pointerIcon
            override fun onPointerCaptureChange(hasCapture: Boolean) { super.onPointerCaptureChange(hasCapture); mouseCaptureChanged(hasCapture) }
        }
        video = SurfaceView(this).apply {
            holder.addCallback(this@StreamActivity)
            pivotX = 0f; pivotY = 0f
        }
        stage.addView(video, FrameLayout.LayoutParams(1, 1))
        // Frozen last frame: the image copies the video's layout and transform
        // (relayout/applyViewport), so letterbox and zoom match exactly.
        frozenLayer = FrameLayout(this).apply { visibility = View.GONE }
        frozenImage = ImageView(this).apply {
            scaleType = ImageView.ScaleType.FIT_XY
            pivotX = 0f; pivotY = 0f
            setColorFilter(FROZEN_DIM, PorterDuff.Mode.SRC_ATOP)
        }
        frozenLayer.addView(frozenImage, FrameLayout.LayoutParams(1, 1))
        frozenLabel = TextView(this).apply { styleStatusPill(this); setTextColor(STATUS_PENDING); setText(R.string.status_reconnecting) }
        frozenLayer.addView(frozenLabel, FrameLayout.LayoutParams(ViewGroup.LayoutParams.WRAP_CONTENT, ViewGroup.LayoutParams.WRAP_CONTENT, Gravity.CENTER))
        stage.addView(frozenLayer, FrameLayout.LayoutParams(ViewGroup.LayoutParams.MATCH_PARENT, ViewGroup.LayoutParams.MATCH_PARENT))
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
            styleStatusPill(this)
            setOnClickListener { onStatusTapped() }
        }
        root.addView(status, FrameLayout.LayoutParams(ViewGroup.LayoutParams.WRAP_CONTENT, ViewGroup.LayoutParams.WRAP_CONTENT, Gravity.TOP or Gravity.START).apply {
            topMargin = dp(8); leftMargin = dp(8)
        })
        secureBanner = SecureBanner(this, root) {
            session?.denySecurePrompt()
            secureBanner.denySent()
        }

        // The dock: transfer rows over the quick-action bar. It never covers the
        // video: the stage (and so the viewport, letterbox and touch mapping) is
        // the area above the dock. Root padding already removes the soft keyboard
        // and, outside full screen, the system bars, so the bar sits on the
        // keyboard and the stream above.
        dock = LinearLayout(this).apply { orientation = LinearLayout.VERTICAL }
        transferDock = TransferDock(this, dock, this)
        quickBar = QuickBar(this, root, dock, this) // adds its scrim to root and the bar last in the dock
        root.addView(dock, FrameLayout.LayoutParams(ViewGroup.LayoutParams.MATCH_PARENT, ViewGroup.LayoutParams.WRAP_CONTENT, Gravity.BOTTOM))
        (stage.layoutParams as FrameLayout.LayoutParams).bottomMargin = quickBar.height
        dock.addOnLayoutChangeListener { _, _, top, _, bottom, _, _, _, _ ->
            val height = bottom - top
            val lp = stage.layoutParams as FrameLayout.LayoutParams
            if (height > 0 && lp.bottomMargin != height) stage.post { lp.bottomMargin = height; stage.layoutParams = lp }
        }

        root.setOnApplyWindowInsetsListener { v, insets -> applyInsets(v, insets); insets }
        setContentView(root)
    }

    /**
     * Edge-to-edge (targetSdk 35): keep the bar above the soft keyboard, and
     * clear of the system bars when full screen is off.
     */
    @Suppress("DEPRECATION")
    private fun applyInsets(v: View, insets: WindowInsets) {
        var left = 0; var top = 0; var right = 0; var bottom = 0
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.R) {
            bottom = insets.getInsets(WindowInsets.Type.ime()).bottom
            if (!immersive) {
                val bars = insets.getInsets(WindowInsets.Type.systemBars() or WindowInsets.Type.displayCutout())
                left = bars.left; top = bars.top; right = bars.right; bottom = maxOf(bottom, bars.bottom)
            }
            // The user can close the keyboard with the system Back gesture.
            val imeVisible = insets.isVisible(WindowInsets.Type.ime())
            if (keyboardShown && !imeVisible && v.isAttachedToWindow && hasWindowFocus()) main.post { onImeClosed() }
        } else if (!immersive) {
            left = insets.systemWindowInsetLeft; top = insets.systemWindowInsetTop
            right = insets.systemWindowInsetRight; bottom = insets.systemWindowInsetBottom
        }
        v.setPadding(left, top, right, bottom)
    }

    private fun onImeClosed() {
        if (!keyboardShown) return
        keyboardShown = false
        keyboardView.resetContext()
        keyboardView.clearFocus()
        quickBar.render()
    }

    private fun enterImmersive() {
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

    private fun exitImmersive() {
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.R) {
            window.insetsController?.show(WindowInsets.Type.systemBars())
        } else {
            @Suppress("DEPRECATION")
            window.decorView.systemUiVisibility = View.SYSTEM_UI_FLAG_LAYOUT_FULLSCREEN or
                View.SYSTEM_UI_FLAG_LAYOUT_HIDE_NAVIGATION or View.SYSTEM_UI_FLAG_LAYOUT_STABLE
        }
    }

    override fun onWindowFocusChanged(hasFocus: Boolean) {
        super.onWindowFocusChanged(hasFocus)
        remoteClipboard.foreground = resumed && hasFocus
        if (remoteClipboard.foreground) { observeLocalClipboard(); applyRemoteClipboard() }
        if (hasFocus) { if (immersive) enterImmersive() } else if (stage.hasPointerCapture()) stage.releasePointerCapture()
    }

    // ---- session lifecycle -----------------------------------------------------
    private fun startSession() {
        if (session != null) return
        val s = Session(config, { Prefs.cookieHeader(config) }, Session.chooseCodec(prefs.codec), this)
        session = s
        input = RemoteInput(s, cursorReconciler::noteSent, ::scheduleMotionFlush)
        surface?.let { s.decoder.setSurface(it) }
        s.setFollow(prefs.followWindow)
        s.setStreamMode(gameMode)
        cursorReconciler.reset()
        onCursorType(CursorKind.Arrow)
        renderCursor()
        s.start()
        defaultMonitorChecked = false
        applyKeepScreenOn()
        startKeepAlive()
    }

    /**
     * Background survival: the foreground service starts with the session,
     * while this screen is in the foreground (Android refuses it later).
     */
    private fun startKeepAlive() {
        if (session == null || !prefs.stayConnected || KeepAlive.active) return
        askNotifications()
        SessionService.start(this, config.base.host)
    }

    /** Android 13+: the session notification and download notices need the permission; asked once per screen. */
    private fun askNotifications() {
        if (Build.VERSION.SDK_INT >= 33 && !DownloadNotifier.canNotify(this) && !askedNotifications) {
            askedNotifications = true
            requestPermissions(arrayOf(Manifest.permission.POST_NOTIFICATIONS), REQUEST_NOTIFY)
        }
    }

    /** Disconnect from the session notification. */
    override fun onKeepAliveDisconnect() {
        disconnect()
    }

    /** The service was refused or reached Android's time limit: back to the 30 s background stop. */
    override fun onKeepAliveEnded() {
        if (!stopped || session == null) return
        session?.setForeground(false)
        main.removeCallbacks(backgroundDisconnect)
        main.postDelayed(backgroundDisconnect, BACKGROUND_GRACE_MS)
    }

    private fun applyKeepScreenOn() {
        if (session != null && prefs.keepScreenOn) window.addFlags(WindowManager.LayoutParams.FLAG_KEEP_SCREEN_ON)
        else window.clearFlags(WindowManager.LayoutParams.FLAG_KEEP_SCREEN_ON)
    }

    private fun stopSession() {
        keyboardView.resetContext()
        remoteClipboard.localAction()
        quickBar.release()
        // Transfers belong to the session: its end cancels them.
        files?.cancelAll()
        currentOffer = null
        transferDock.clear()
        input?.releaseAll()
        metaSync.reset()
        session?.stop()
        if (KeepAlive.active) SessionService.stop(this)
        session = null
        input = null
        followCapable = false
        hostFollow = null
        cursorReconciler.reset()
        onCursorType(CursorKind.Arrow)
        renderCursor()
        window.clearFlags(WindowManager.LayoutParams.FLAG_KEEP_SCREEN_ON)
        onStatus(Session.Status(Session.State.CONNECTING, getString(R.string.status_paused)))
    }

    override fun onStart() {
        super.onStart()
        stopped = false
        foregroundAt = SystemClock.elapsedRealtime()
        main.removeCallbacks(backgroundDisconnect)
        if (session == null) startSession()
        // Reconnect now (backoff reset) if a retry was held or waiting.
        session?.setForeground(true)
    }

    override fun onPause() {
        observeLocalClipboard()
        resumed = false
        remoteClipboard.foreground = false
        captureFrozenFrame() // the surface still holds the last frame here
        super.onPause()
        quickBar.release()
        input?.releaseAll()
        metaSync.reset()
        cursorReconciler.reset()
        keyboardView.resetContext()
        latchSet.clear(); activeLatched.clear(); latchesChanged()
    }

    override fun onResume() {
        super.onResume()
        resumed = true
        remoteClipboard.foreground = hasWindowFocus()
        if (remoteClipboard.foreground) { observeLocalClipboard(); applyRemoteClipboard() }
    }

    override fun onStop() {
        super.onStop()
        stopped = true
        // The foreground service keeps the session, its transfers and reconnects alive.
        if (session != null && KeepAlive.active) return
        // Without it: keep the session for a short background trip (app switch), then let go.
        // Meanwhile it does not reconnect lost sockets.
        session?.setForeground(false)
        main.postDelayed(backgroundDisconnect, BACKGROUND_GRACE_MS)
    }

    // ---- frozen last frame ---------------------------------------------------------
    /** Copies the current video frame (asynchronously) for display after a background trip. */
    private fun captureFrozenFrame() {
        val target = surface
        if (videoWidth <= 0 || videoHeight <= 0 || target == null || !target.isValid || session == null) return
        val token = frozen.captureStarted() ?: return
        // No larger than the stream nor than it appears on screen (zoom included).
        val scale = minOf(1f, video.width * video.scaleX / videoWidth, video.height * video.scaleY / videoHeight).coerceAtLeast(0.1f)
        val bitmap = try {
            Bitmap.createBitmap((videoWidth * scale).roundToInt().coerceAtLeast(1), (videoHeight * scale).roundToInt().coerceAtLeast(1), Bitmap.Config.ARGB_8888)
        } catch (_: OutOfMemoryError) { frozen.captured(token, null); return }
        try {
            PixelCopy.request(video, bitmap, { result ->
                if (result != PixelCopy.SUCCESS) Log.i(TAG, "frozen frame capture failed: $result")
                if (isDestroyed) return@request
                if (frozen.captured(token, bitmap.takeIf { result == PixelCopy.SUCCESS })) showFrozen()
            }, main)
        } catch (e: IllegalArgumentException) {
            Log.i(TAG, "frozen frame capture failed", e)
            frozen.captured(token, null)
        }
    }

    private fun showFrozen() {
        val image = frozen.snapshot ?: return
        frozenLayer.animate().cancel()
        frozenImage.setImageBitmap(image)
        frozenLayer.alpha = 1f
        frozenLayer.visibility = View.VISIBLE
        updateFrozenLabel()
    }

    private fun hideFrozen(fade: Boolean) {
        frozenLayer.animate().cancel()
        if (frozenLayer.visibility != View.VISIBLE) return
        val done = Runnable {
            frozenLayer.visibility = View.GONE
            frozenImage.setImageDrawable(null)
        }
        if (fade) frozenLayer.animate().alpha(0f).setDuration(FROZEN_FADE_MS).withEndAction(done).start() else done.run()
    }

    /** "Reconnecting…" on the frozen frame, unless the status line reports an error. */
    private fun updateFrozenLabel() {
        frozenLabel.visibility = if (lastStatus?.state == Session.State.ERROR) View.GONE else View.VISIBLE
    }

    /** An explicit disconnect or sign-out: the next connection must not show this desktop. */
    private fun clearFrozen() {
        frozen.clear()
        hideFrozen(fade = false)
    }

    override fun onDestroy() {
        clipboardManager.removePrimaryClipChangedListener(clipboardChanged)
        main.removeCallbacksAndMessages(null)
        if (::quickBar.isInitialized) {
            quickBar.onDestroy()
            transferDock.onDestroy()
            stopSession()
        }
        if (KeepAlive.owner === this) KeepAlive.owner = null
        files?.shutdown()
        files = null
        super.onDestroy()
    }

    @Deprecated("Deprecated in Java")
    override fun onBackPressed() {
        when {
            settingsView != null -> closeSettings()
            quickBar.dismiss() -> {}
            keyboardShown -> toggleKeyboard()
            else -> AlertDialog.Builder(this)
                .setMessage(R.string.confirm_disconnect)
                .setPositiveButton(R.string.key_disconnect) { _, _ -> disconnect() }
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
        if (frozen.surfaceLost()) showFrozen()
    }

    // ---- Session.Listener (main thread) --------------------------------------------
    override fun onStatus(status: Session.Status) {
        lastStatus = status
        this.status.text = status.text
        this.status.setTextColor(
            if (status.inputWarning) STATUS_PENDING else when (status.state) {
                Session.State.CONNECTED, Session.State.STATIC -> 0xFFB8F5C8.toInt()
                Session.State.ERROR -> 0xFFFFB4A8.toInt()
                else -> STATUS_PENDING
            },
        )
        if (::frozenLabel.isInitialized) updateFrozenLabel()
    }

    override fun onLiveFrame() {
        val wasFrozen = frozen.livePresented()
        if (wasFrozen) hideFrozen(fade = true)
        if (foregroundAt != 0L) {
            Log.i(TAG, "foreground to first live frame ${SystemClock.elapsedRealtime() - foregroundAt} ms (frozen frame shown=$wasFrozen)")
            foregroundAt = 0L
        }
    }

    override fun onAuthRequired() {
        stopSession()
        // Shares that have not gone out yet come back after sign-in.
        val waiting = pendingUploads.filter { it.second == FileWire.PURPOSE_SAVE }.flatMap { it.first }
        startActivity(PendingShares.attach(Intent(this, LoginActivity::class.java).putExtra(LoginActivity.EXTRA_FORCE, true), waiting))
        finish()
    }

    override fun onCursor(state: CursorState) {
        cursorVisible = state.visible
        // A late echo never rewinds newer queued or unacknowledged movement.
        val authoritative = cursorReconciler.apply(state, gestures.active, input?.hasPendingCursorMotion() == true)
        if (!state.inBounds) { cursorView.update(0f, 0f, false); return }
        if (!authoritative) return
        cursorX = state.x / 65535f
        cursorY = state.y / 65535f
        renderCursor()
    }

    override fun onCursorType(kind: CursorKind) {
        cursorView.setKind(kind)
        stage.pointerIcon = PointerIcon.getSystemIcon(this, CursorAppearance.systemType(kind))
        // Keep the overlay's position/visibility intact: appearance updates even
        // during touch prediction, without undoing an out-of-bounds position.
    }

    override fun onRemoteClipboard(text: String) {
        remoteClipboard.receive(text)
        applyRemoteClipboard()
    }

    private fun observeLocalClipboard() {
        if (!resumed || !hasWindowFocus()) return
        val before = remoteClipboard.row
        remoteClipboard.observeLocal(clipboardManager.primaryClipDescription?.timestamp)
        if (remoteClipboard.row != before && ::quickBar.isInitialized) quickBar.clipboardChanged()
    }

    private fun applyRemoteClipboard() {
        remoteClipboard.takeAutomatic()?.let { writeRemoteClipboard(it) }
        if (::quickBar.isInitialized) quickBar.clipboardChanged()
    }

    private fun writeRemoteClipboard(text: String) {
        clipboardManager.setPrimaryClip(ClipData.newPlainText("BubblePlay", text))
        // Remember our own write before its asynchronous change callback arrives.
        remoteClipboard.rememberWrite(clipboardManager.primaryClipDescription?.timestamp)
    }

    override fun onMonitors(monitors: List<Session.Monitor>, error: String?) {
        if (error != null) {
            // Only a switch the user asked for gets a toast; the popover shows the error either way.
            if (switchingMonitor != null) Toast.makeText(this, getString(R.string.monitor_failed, error), Toast.LENGTH_LONG).show()
            monitorError = error
            switchingMonitor = null
        } else {
            this.monitors = monitors
            monitorError = null
            val target = switchingMonitor
            if (target != null && monitors.any { it.selector == target && it.selected }) switchingMonitor = null
            if (!defaultMonitorChecked) {
                // Settings -> Default monitor: asked for once per session, when the host has it.
                defaultMonitorChecked = true
                val wanted = prefs.defaultMonitor
                if (wanted.isNotEmpty() && switchingMonitor == null && monitors.any { it.selector == wanted && !it.selected }) switchMonitor(wanted)
            }
        }
        quickBar.monitorsChanged()
        settingsView?.monitorsChanged()
    }

    override fun onVideoSize(width: Int, height: Int) {
        videoWidth = width; videoHeight = height
        video.holder.setFixedSize(width, height)
        relayout()
        // A screen switch usually changes the frame; refresh which one is current.
        if (switchingMonitor != null) session?.requestMonitorList()
    }

    override fun onInputReady() {
        keyboardView.resetContext()
        cursorReconciler.reset()
        // Release-all only if held input was let go during a monitor-transition gate; then resync the cursor.
        input?.resumeAfterTransition()
        input?.cursorSync()
        session?.requestMonitorList()
    }

    override fun onInputLost() {
        keyboardView.resetContext()
        // The host drops held input with its session; mirror that locally so
        // nothing stays "down" and the next packet starts with a release-all.
        quickBar.release()
        input?.releaseAll(send = false)
        metaSync.reset()
        cursorReconciler.reset()
        gestures.cancel()
        main.removeCallbacks(longPress)
        mouseButtons = 0
        onCursorType(CursorKind.Arrow)
        latchSet.clear(); activeLatched.clear(); latchesChanged()
    }

    override fun onSecureDesktop(state: SecureDesktop?) {
        secureBanner.show(state)
    }

    override fun onSecureActionResult(ok: Boolean, reason: String) {
        secureBanner.denyResult(ok, reason)
    }

    override fun onDesktopChanged() {
        keyboardView.resetContext()
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
        val flp = frozenImage.layoutParams as FrameLayout.LayoutParams
        if (flp.width != lp.width || flp.height != lp.height || flp.leftMargin != lp.leftMargin || flp.topMargin != lp.topMargin) {
            flp.width = lp.width; flp.height = lp.height; flp.leftMargin = lp.leftMargin; flp.topMargin = lp.topMargin
            frozenImage.layoutParams = flp
        }
        applyViewport()
    }

    private fun applyViewport() {
        val z = viewport.zoom
        for (view in listOf(video, frozenImage)) {
            view.scaleX = z; view.scaleY = z
            view.translationX = viewport.x + viewport.contentX * (z - 1f)
            view.translationY = viewport.y + viewport.contentY * (z - 1f)
        }
        renderCursor()
        if (viewport.zoomed != barZoomed) { barZoomed = viewport.zoomed; quickBar.render() }
    }

    private fun renderCursor() {
        val (sx, sy) = viewport.toScreen(cursorX, cursorY)
        // Direct touch: the finger is the pointer, so no drawn cursor unless a
        // physical mouse is driving it. Trackpad mode always draws it.
        val show = when {
            session == null -> false
            gestures.mode == TouchMode.TRACKPAD -> true
            else -> mouseActive && cursorVisible
        }
        cursorView.update(sx, sy, show && settingsView == null)
    }

    private fun setMouseActive(active: Boolean) {
        if (mouseActive == active) return
        mouseActive = active
        renderCursor()
    }

    // ---- touch ------------------------------------------------------------------------
    private fun onStageTouch(event: MotionEvent): Boolean {
        if (event.isFromSource(InputDevice.SOURCE_MOUSE)) return onMouseEvent(event)
        if (stage.hasPointerCapture()) stage.releasePointerCapture()
        setMouseActive(false)
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
                val (dx, dy) = viewport.deltaToNormalized(action.dx * pointerGain, action.dy * pointerGain)
                cursorX = (cursorX + dx).coerceIn(0f, 1f); cursorY = (cursorY + dy).coerceIn(0f, 1f)
                remote.absoluteNormalized(cursorX, cursorY)
                // Zoomed trackpad: the view follows the cursor (re-centres after a two-finger nudge).
                viewport.centerOn(cursorX, cursorY)
                applyViewport()
                scheduleMotionFlush()
            }
            is GestureAction.PointAt -> {
                val (nx, ny) = viewport.toNormalized(action.x, action.y)
                cursorX = nx.coerceIn(0f, 1f); cursorY = ny.coerceIn(0f, 1f)
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
            is GestureAction.Zoom -> {
                // Trackpad zoom anchors on the cursor; direct zoom on the pinch focus.
                if (gestures.mode == TouchMode.TRACKPAD) viewport.zoomAround(action.scale, cursorX, cursorY)
                else viewport.zoomBy(action.scale, action.focusX, action.focusY)
                applyViewport()
            }
            is GestureAction.Pan -> { viewport.pan(action.dx, action.dy); applyViewport() }
            GestureAction.LongPressArmed -> stage.performHapticFeedback(HapticFeedbackConstants.LONG_PRESS)
            GestureAction.ResetZoom -> resetZoom()
        }
    }

    private fun buttonsAtCursor(remote: RemoteInput, mask: Int) {
        keyboardView.resetContext()
        remote.flushMotion()
        remote.buttonsAt((cursorX * 65535f).roundToInt(), (cursorY * 65535f).roundToInt(), mask)
        if (remote.hasPendingInput()) scheduleMotionFlush()
    }

    private fun scheduleMotionFlush() {
        if (motionFramePosted) return
        motionFramePosted = true
        Choreographer.getInstance().postFrameCallback(motionFrame)
    }

    // ---- mouse ------------------------------------------------------------------------
    private fun onStageGenericMotion(event: MotionEvent): Boolean {
        // Wheel and touchpad scroll arrive here while the pointer is uncaptured; once captured they
        // go to onCapturedPointer instead, so each scroll sample is routed exactly once.
        if (event.actionMasked == MotionEvent.ACTION_SCROLL && PointerWheel.isPointerSource(event.source)) {
            if (event.isFromSource(InputDevice.SOURCE_MOUSE)) onMouseEvent(event)
            onWheel(event)
            return true
        }
        if (!event.isFromSource(InputDevice.SOURCE_MOUSE)) return false
        return onMouseEvent(event)
    }

    /** Wheel or touchpad scroll axes (notches) to WHEEL_DELTA units, sent as a wheel-only motion. */
    private fun onWheel(event: MotionEvent) {
        val remote = input ?: return
        val (wx, wy) = wheel.add(
            PointerWheel.units(event.getAxisValue(MotionEvent.AXIS_HSCROLL)),
            PointerWheel.units(event.getAxisValue(MotionEvent.AXIS_VSCROLL)),
        )
        if (wx != 0 || wy != 0) { remote.motion(0, 0, wx, wy); scheduleMotionFlush() }
    }

    /**
     * A physical mouse outside pointer capture. Work mode: it points where it is
     * over the stream (absolute packets, the system pointer shows the remote
     * shape). Game mode: a click on the stream captures it (that click is not
     * sent); captured motion is relative ([onCapturedPointer]).
     */
    private fun onMouseEvent(event: MotionEvent): Boolean {
        if (gameMode) {
            val press = event.actionMasked == MotionEvent.ACTION_DOWN || event.actionMasked == MotionEvent.ACTION_BUTTON_PRESS
            if (press && !stage.hasPointerCapture() && hasWindowFocus() && settingsView == null) {
                captureClick.captureRequested(mouseMask(event.buttonState))
                stage.requestPointerCapture()
            }
            return true
        }
        pointAbsolute(event)
        return true
    }

    /** Work mode: the remote pointer follows the mouse's position over the stream; buttons go with that position. */
    private fun pointAbsolute(event: MotionEvent) {
        val remote = input ?: return
        if (videoWidth <= 0 || videoHeight <= 0) return
        setMouseActive(false)
        val (nx, ny) = viewport.toNormalized(event.x, event.y)
        val x = nx.coerceIn(0f, 1f)
        val y = ny.coerceIn(0f, 1f)
        if (x != cursorX || y != cursorY) {
            cursorX = x; cursorY = y
            remote.absoluteNormalized(x, y)
            renderCursor()
            scheduleMotionFlush()
        }
        val mask = mouseMask(event.buttonState)
        if (mask != mouseButtons) {
            mouseButtons = mask
            buttonsAtCursor(remote, mask)
        }
    }

    /** Pointer capture began or ended (Esc, a touch, focus loss, settings, or the mode changing). */
    private fun mouseCaptureChanged(captured: Boolean) {
        if (captured) {
            transferDock.showNotice(getString(if (prefs.keyboardLock) R.string.mouse_captured_hold_esc else R.string.mouse_captured_esc))
            return
        }
        main.removeCallbacks(escapeRelease)
        captureClick.reset()
        setMouseActive(false)
        if (mouseButtons != 0) {
            mouseButtons = 0
            input?.let { it.flushMotion(); it.buttons(0); if (it.hasPendingInput()) scheduleMotionFlush() }
        }
    }

    private fun releaseMouseCapture() {
        if (stage.hasPointerCapture()) stage.releasePointerCapture()
    }

    /** Work or game pointer mode for a physical mouse; also tells the host (game streams at up to 60 fps). */
    private fun applyPointerMode() {
        val game = prefs.gameMode
        if (game == gameMode) return
        gameMode = game
        session?.setStreamMode(game)
        if (!game) releaseMouseCapture()
    }

    private fun onCapturedPointer(event: MotionEvent): Boolean {
        setMouseActive(true)
        val remote = input ?: return true
        when (event.actionMasked) {
            MotionEvent.ACTION_MOVE, MotionEvent.ACTION_HOVER_MOVE -> {
                var dx = event.x
                var dy = event.y
                for (h in 0 until event.historySize) { dx += event.getHistoricalX(h); dy += event.getHistoricalY(h) }
                remote.motion(dx.roundToInt(), dy.roundToInt())
                scheduleMotionFlush()
            }
            MotionEvent.ACTION_SCROLL -> onWheel(event)
        }
        val mask = captureClick.filter(mouseMask(event.buttonState))
        if (mask != mouseButtons) {
            keyboardView.resetContext()
            mouseButtons = mask
            remote.flushMotion()
            remote.buttons(mask)
            if (remote.hasPendingInput()) scheduleMotionFlush()
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
        // While settings are open, keys belong to the settings view (text field, focus, Back).
        if (hardware && settingsView == null && event.isFromSource(InputDevice.SOURCE_KEYBOARD) && KeyMap.ANDROID_TO_CODE.containsKey(event.keyCode)) {
            if (event.keyCode == KeyEvent.KEYCODE_ESCAPE && escapeForCapture(event)) return true
            sendHardwareKey(event)
            return true
        }
        return super.dispatchKeyEvent(event)
    }

    /**
     * Esc while the mouse is captured. Keyboard lock off: Esc releases the
     * mouse and stays on the phone (down and up). On: Esc goes to the PC, and
     * holding it for [ESC_HOLD_MS] releases the mouse. True when consumed here.
     */
    private fun escapeForCapture(event: KeyEvent): Boolean {
        if (event.action == KeyEvent.ACTION_UP) main.removeCallbacks(escapeRelease)
        if (escapeSwallowed) {
            if (event.action == KeyEvent.ACTION_UP) escapeSwallowed = false
            return true
        }
        if (!stage.hasPointerCapture()) return false
        if (!prefs.keyboardLock) {
            if (event.action == KeyEvent.ACTION_DOWN) { escapeSwallowed = true; releaseMouseCapture() }
            return true
        }
        if (event.action == KeyEvent.ACTION_DOWN && event.repeatCount == 0) main.postDelayed(escapeRelease, ESC_HOLD_MS)
        return false
    }

    /** A hardware key; with keyboard lock on, Win combinations Android delivers without their Win press get one ([MetaSync]). */
    private fun sendHardwareKey(event: KeyEvent) {
        val code = KeyMap.ANDROID_TO_CODE[event.keyCode] ?: return
        val up = event.action == KeyEvent.ACTION_UP
        if (code == "MetaLeft" || code == "MetaRight") metaSync.observeMeta(code, up)
        if (!prefs.keyboardLock || code in KeyMap.MODIFIERS || input == null) { sendKey(event); return }
        if (event.action == KeyEvent.ACTION_DOWN && metaSync.beforeDown(event.isMetaPressed)) input?.key(MetaSync.META, false)
        sendKey(event)
        if (up && metaSync.afterUp()) input?.key(MetaSync.META, true)
    }

    /** Soft-keyboard key events (Enter, Backspace, some IMEs send every key). */
    override fun onImeKey(event: KeyEvent): Boolean {
        if (session?.inputOpen() != true) return false
        val code = KeyMap.ANDROID_TO_CODE[event.keyCode]
        val unicode = event.getUnicodeChar(event.metaState)
        val printable = unicode > 0 && !Character.isISOControl(unicode) && code != "Space"
        if (printable && latchSet.isEmpty() && !event.isCtrlPressed && !event.isAltPressed) {
            // A character from the IME: send the character, not a US-layout key.
            return event.action != KeyEvent.ACTION_DOWN || input?.text(String(Character.toChars(unicode))) == true
        }
        if (code == null) return false
        return sendKey(event, imeOrigin = true)
    }

    @Suppress("DEPRECATION") // ACTION_MULTIPLE still arrives from some IMEs and HID stacks.
    private fun sendKey(event: KeyEvent, imeOrigin: Boolean = false): Boolean {
        val code = KeyMap.ANDROID_TO_CODE[event.keyCode] ?: return false
        if (input == null) return false
        if (code == "Backspace" || code == "Delete") {
            // Injected held keys do not repeat on Windows. Every Android repeat
            // is a complete tap, including repeats received with an empty IME.
            if (event.action == KeyEvent.ACTION_UP) return true
            val count = when (event.action) {
                KeyEvent.ACTION_DOWN -> 1
                KeyEvent.ACTION_MULTIPLE -> event.repeatCount.coerceAtLeast(1)
                else -> return false
            }
            if (!imeOrigin) keyboardView.resetContext() // Hardware deletion abandons the known IME segment.
            return RemoteKeyRepeats.tap(count) { up ->
                if (up) keyUpChecked(code) else keyDownChecked(code, resetImeContext = false)
            }
        }
        return when (event.action) {
            KeyEvent.ACTION_DOWN -> if (event.repeatCount == 0) keyDownChecked(code, resetImeContext = !imeOrigin) else input?.key(code, false) == true
            KeyEvent.ACTION_UP -> keyUpChecked(code)
            KeyEvent.ACTION_MULTIPLE -> if (imeOrigin && (code == "Enter" || code == "Tab")) {
                RemoteKeyRepeats.tap(1) { up ->
                    if (up) keyUpChecked(code) else keyDownChecked(code, resetImeContext = false)
                }
            } else event.characters?.let { input?.text(it) } == true
            else -> false
        }
    }

    override fun onImeEdit(deleteCount: Int, insert: String): Boolean {
        val remote = input ?: return false
        if (session?.inputOpen() != true) return false
        return ImeTextSender(
            text = remote::text,
            rawTap = { code -> remote.key(code, false) && remote.key(code, true) },
            keyTap = { code -> keyDownChecked(code, resetImeContext = false) && keyUpChecked(code) },
            hasLatch = { latchSet.isNotEmpty() },
            requestReset = { keyboardView.requestDeferredResetContext() },
        ).send(deleteCount, insert)
    }

    override fun onImeDeleteForward(count: Int): Boolean {
        val remote = input ?: return false
        if (session?.inputOpen() != true) return false
        repeat(count) {
            if (!remote.key("Delete", false) || !remote.key("Delete", true)) {
                keyboardView.requestDeferredResetContext()
                return false
            }
        }
        return true
    }

    // ---- QuickBar.Host: keys ------------------------------------------------------------------
    /** Tap with any latched modifiers, then clear the latch. */
    override fun tapKey(code: String) {
        keyDown(code)
        keyUp(code)
    }

    /**
     * Key press. A non-modifier key takes the latched (sticky) modifiers with
     * it: they go down first and come up after this key's release.
     */
    override fun keyDown(code: String) {
        keyDownChecked(code)
    }

    private fun keyDownChecked(code: String, resetImeContext: Boolean = true): Boolean {
        if (resetImeContext && code !in KeyMap.MODIFIERS) keyboardView.resetContext()
        if (input == null) return false
        return latchedKeySender.down(code)
    }

    /** Typematic repeat: Windows does not auto-repeat injected keys, so resend the press. */
    override fun keyRepeat(code: String) {
        if (code !in KeyMap.MODIFIERS) keyboardView.resetContext()
        input?.key(code, false)
    }

    override fun keyUp(code: String) {
        keyUpChecked(code)
    }

    private fun keyUpChecked(code: String): Boolean {
        if (input == null) return false
        return latchedKeySender.up(code)
    }

    /** Sticky Ctrl/Alt/Shift/Win: held for the next key; tapping it again lets go (sends nothing). */
    override fun toggleLatch(code: String) {
        if (!latchSet.remove(code)) latchSet.add(code)
        latchesChanged()
    }

    /** Shortcut chords are exact: they ignore (and keep) any sticky modifiers. */
    override fun sendChord(chord: Chord) {
        keyboardView.resetContext()
        val remote = input ?: return
        chord.mods.forEach { remote.key(it, false) }
        remote.tap(chord.key)
        chord.mods.asReversed().forEach { remote.key(it, true) }
    }

    private fun latchesChanged() {
        if (::quickBar.isInitialized) quickBar.render()
    }

    override fun toggleKeyboard() {
        val imm = getSystemService(Context.INPUT_METHOD_SERVICE) as InputMethodManager
        keyboardShown = !keyboardShown
        if (keyboardShown) {
            keyboardView.requestFocus()
            imm.restartInput(keyboardView)
            imm.showSoftInput(keyboardView, InputMethodManager.SHOW_IMPLICIT)
        } else {
            keyboardView.resetContext()
            imm.hideSoftInputFromWindow(keyboardView.windowToken, 0)
            keyboardView.clearFocus()
        }
        quickBar.render()
    }

    // ---- QuickBar.Host: everything else --------------------------------------------------------
    override fun toggleTouchMode() {
        gestures.cancel().let { apply(it) }
        // For this session only; the default is in Settings.
        gestures.mode = if (gestures.mode == TouchMode.TRACKPAD) TouchMode.DIRECT else TouchMode.TRACKPAD
        renderCursor()
    }

    override fun resetZoom() {
        viewport.resetZoom()
        applyViewport()
    }

    override fun toggleImmersive() {
        immersive = !immersive
        if (immersive) enterImmersive() else exitImmersive()
        root.requestApplyInsets()
    }

    override fun refreshMonitors() {
        session?.requestMonitorList()
    }

    override fun switchMonitor(selector: String) {
        keyboardView.resetContext()
        switchingMonitor = selector
        monitorError = null
        session?.requestMonitor(selector)
        main.removeCallbacks(monitorRefresh)
        main.postDelayed(monitorRefresh, MONITOR_REFRESH_MS)
        quickBar.monitorsChanged()
    }

    override fun toggleFollowWindow() {
        val enabled = !(followWindow ?: return)
        prefs.followWindow = enabled
        hostFollow = null
        session?.setFollow(enabled)
        quickBar.monitorsChanged()
    }

    override fun onFollowCapability(enabled: Boolean) {
        followCapable = enabled
        hostFollow = null
        quickBar.monitorsChanged()
    }

    override fun onFollowState(follow: Boolean) {
        if (hostFollow == follow) return
        hostFollow = follow
        quickBar.monitorsChanged()
    }

    override fun pastePhoneClipboard() {
        keyboardView.resetContext()
        remoteClipboard.localAction()
        quickBar.clipboardChanged()
        val manager = getSystemService(Context.CLIPBOARD_SERVICE) as ClipboardManager
        val item = manager.primaryClip?.takeIf { it.itemCount > 0 }?.getItemAt(0)
        // An image or file on the phone clipboard is uploaded and pasted on the PC.
        val uri = item?.uri
        if (uri != null && fileTransfer && ContentResolver.SCHEME_CONTENT == uri.scheme) {
            enqueueUpload(listOf(uri), FileWire.PURPOSE_PASTE); return
        }
        val text = item?.coerceToText(this)?.toString()
        if (text.isNullOrEmpty()) { Toast.makeText(this, R.string.clipboard_empty, Toast.LENGTH_SHORT).show(); return }
        val remote = input ?: return
        val ok = if (session?.clipboardEnabled() == true) remote.clipboard(text, paste = true) else remote.text(text)
        if (!ok) Toast.makeText(this, R.string.paste_failed, Toast.LENGTH_SHORT).show()
    }

    override fun copyPcClipboard() {
        if (!resumed || !hasWindowFocus()) return
        val text = remoteClipboard.take() ?: return
        writeRemoteClipboard(text)
        quickBar.clipboardChanged()
    }

    override fun disconnect() {
        clearFrozen()
        finish()
    }

    // ---- settings ------------------------------------------------------------------------------
    /** Settings open over the stream; the session (video, transfers) keeps running. */
    override fun openSettings() {
        if (settingsView != null) return
        quickBar.dismiss()
        if (keyboardShown) toggleKeyboard()
        quickBar.release()
        input?.releaseAll()
        gestures.cancel().let { apply(it) }
        if (stage.hasPointerCapture()) stage.releasePointerCapture()
        val view = SettingsView(this, prefs, this)
        view.setOnApplyWindowInsetsListener { v, insets -> padForCutout(v, insets); insets }
        settingsView = view
        root.addView(view, FrameLayout.LayoutParams(ViewGroup.LayoutParams.MATCH_PARENT, ViewGroup.LayoutParams.MATCH_PARENT))
        view.requestApplyInsets()
        renderCursor()
        session?.requestMonitorList()
    }

    override fun closeSettings() {
        val view = settingsView ?: return
        (getSystemService(Context.INPUT_METHOD_SERVICE) as InputMethodManager).hideSoftInputFromWindow(view.windowToken, 0)
        root.removeView(view)
        settingsView = null
        renderCursor()
    }

    /** Full screen leaves the cutout to the stream; the settings view keeps its content clear of it. */
    private fun padForCutout(v: View, insets: WindowInsets) {
        val cutout = insets.displayCutout
        v.setPadding(
            maxOf(0, (cutout?.safeInsetLeft ?: 0) - root.paddingLeft),
            maxOf(0, (cutout?.safeInsetTop ?: 0) - root.paddingTop),
            maxOf(0, (cutout?.safeInsetRight ?: 0) - root.paddingRight),
            0,
        )
    }

    override val settingsConfig: ServerConfig get() = config
    override val inSession: Boolean get() = session != null
    override val settingsMonitors: List<Session.Monitor> get() = if (session != null) monitors else emptyList()
    override fun diagnostics(): Session.Diagnostics? = session?.diagnostics
    override fun probeRoundTrip() { session?.probeRoundTrip() }

    override fun onSettingChanged(setting: SettingsView.Setting) {
        when (setting) {
            SettingsView.Setting.TOUCH_MODE -> {
                val mode = if (prefs.directTouch) TouchMode.DIRECT else TouchMode.TRACKPAD
                if (gestures.mode != mode) { gestures.cancel().let { apply(it) }; gestures.mode = mode; renderCursor(); quickBar.render() }
            }
            SettingsView.Setting.POINTER_SPEED -> pointerGain = PointerSpeed.gain(prefs.pointerSpeed)
            SettingsView.Setting.TAP_TO_CLICK -> gestures.tapToClick = prefs.tapToClick
            SettingsView.Setting.POINTER_MODE -> applyPointerMode()
            SettingsView.Setting.KEYBOARD_LOCK -> main.removeCallbacks(escapeRelease)
            SettingsView.Setting.BACKGROUND -> if (prefs.stayConnected) startKeepAlive() else if (KeepAlive.active) SessionService.stop(this)
            SettingsView.Setting.FULLSCREEN -> if (immersive != prefs.startFullscreen) toggleImmersive()
            SettingsView.Setting.KEEP_SCREEN_ON -> applyKeepScreenOn()
            SettingsView.Setting.DEFAULT_MONITOR -> {
                val wanted = prefs.defaultMonitor
                if (session != null && wanted.isNotEmpty() && monitors.any { it.selector == wanted && !it.selected }) switchMonitor(wanted)
            }
            SettingsView.Setting.CODEC -> {
                // The codec is fixed for a session: reconnect with the new one (transfers in flight end with it).
                val running = session ?: return
                if (Session.chooseCodec(prefs.codec) == running.preferredCodec) return
                stopSession()
                startSession()
                transferDock.showNotice(getString(R.string.settings_codec_reconnecting))
            }
            SettingsView.Setting.SERVER -> {} // the next connection uses it
            SettingsView.Setting.AUTO_COPY_REMOTE_CLIPBOARD -> {
                remoteClipboard.automatic = prefs.autoCopyRemoteClipboard
                observeLocalClipboard()
                applyRemoteClipboard()
            }
        }
    }

    override fun signOut() {
        closeSettings()
        clearFrozen()
        stopSession()
        Prefs.signOut(config) {
            startActivity(Intent(this, MainActivity::class.java)
                .addFlags(Intent.FLAG_ACTIVITY_CLEAR_TOP or Intent.FLAG_ACTIVITY_SINGLE_TOP)
                .putExtra(MainActivity.EXTRA_SIGNED_OUT, true))
            finish()
        }
    }

    // ---- file transfer ---------------------------------------------------------------------------
    override fun attachFiles() {
        val pick = Intent(Intent.ACTION_OPEN_DOCUMENT)
            .addCategory(Intent.CATEGORY_OPENABLE)
            .setType("*/*")
            .putExtra(Intent.EXTRA_ALLOW_MULTIPLE, true)
        try {
            @Suppress("DEPRECATION")
            startActivityForResult(pick, REQUEST_ATTACH)
        } catch (_: ActivityNotFoundException) {
            Toast.makeText(this, R.string.file_no_picker, Toast.LENGTH_SHORT).show()
        }
    }

    @Deprecated("Deprecated in Java")
    override fun onActivityResult(requestCode: Int, resultCode: Int, data: Intent?) {
        @Suppress("DEPRECATION")
        super.onActivityResult(requestCode, resultCode, data)
        if (requestCode != REQUEST_ATTACH || resultCode != RESULT_OK || data == null) return
        val uris = ArrayList<Uri>()
        data.clipData?.let { c -> for (i in 0 until c.itemCount) c.getItemAt(i).uri?.let { uris += it } }
        if (uris.isEmpty()) data.data?.let { uris += it }
        if (uris.size > FileWire.MAX_BATCH) {
            Toast.makeText(this, resources.getQuantityString(R.plurals.file_too_many, uris.size - FileWire.MAX_BATCH, uris.size - FileWire.MAX_BATCH), Toast.LENGTH_LONG).show()
        }
        if (uris.isNotEmpty()) enqueueUpload(uris.distinct().take(FileWire.MAX_BATCH), FileWire.PURPOSE_SAVE)
    }

    private fun enqueueUpload(uris: List<Uri>, purpose: String) {
        when (fileCapability) {
            true -> files?.upload(uris, purpose)
            false -> Toast.makeText(this, R.string.file_unsupported, Toast.LENGTH_LONG).show()
            null -> {
                pendingUploads += uris to purpose
                transferDock.showNotice(getString(R.string.file_waiting_pc))
            }
        }
    }

    override fun onFileCapability(enabled: Boolean?) {
        fileCapability = enabled
        files?.setEnabled(enabled == true)
        quickBar.render()
        when (enabled) {
            true -> {
                val waiting = pendingUploads.toList()
                pendingUploads.clear()
                waiting.forEach { (uris, purpose) -> files?.upload(uris, purpose) }
            }
            false -> {
                if (pendingUploads.isNotEmpty()) Toast.makeText(this, R.string.file_unsupported, Toast.LENGTH_LONG).show()
                pendingUploads.clear()
                currentOffer = null
                quickBar.dismiss()
                transferDock.clear()
            }
            null -> { currentOffer = null; transferDock.showOffer(null) }
        }
    }

    override fun onFileEpoch(epoch: String?) {
        if (epoch == fileEpoch) return
        fileEpoch = epoch
        files?.epochChanged()
    }

    override fun onFileOffer(offer: FileWire.Offer) {
        if (fileCapability != true) return
        // A new offer replaces the previous one; no items withdraws it.
        currentOffer = offer.takeIf { it.items.isNotEmpty() }
        transferDock.showOffer(currentOffer)
    }

    override fun downloadOffer() {
        val offer = currentOffer ?: return
        currentOffer = null
        transferDock.showOffer(null)
        askNotifications()
        files?.download(offer)
    }

    override fun dismissOffer() {
        currentOffer = null
        transferDock.showOffer(null)
    }

    override fun cancelTransfer(direction: FileTransfers.Direction) {
        files?.cancel(direction)
    }

    override fun openDownload(uri: Uri, mime: String) = launch(DownloadNotifier.openIntent(uri, mime))

    override fun shareDownload(uri: Uri, mime: String) = launch(DownloadNotifier.shareIntent(this, uri, mime))

    private fun launch(intent: Intent) {
        try { startActivity(intent) } catch (_: ActivityNotFoundException) {
            Toast.makeText(this, R.string.file_no_app, Toast.LENGTH_SHORT).show()
        }
    }

    private fun fileError(error: String): String = when (error) {
        FileTransfers.ERR_CONNECTION -> getString(R.string.file_err_connection)
        FileTransfers.ERR_AUTH -> getString(R.string.file_err_auth)
        FileTransfers.ERR_UNAVAILABLE -> getString(R.string.file_err_unavailable)
        FileTransfers.ERR_TOO_LARGE -> getString(R.string.file_err_too_large)
        FileTransfers.ERR_UNREADABLE -> getString(R.string.file_err_unreadable)
        FileTransfers.ERR_SIZE_CHANGED -> getString(R.string.file_err_size_changed)
        FileTransfers.ERR_STORAGE -> getString(R.string.file_err_storage)
        ReceiveCheck.ERR_HASH, ReceiveCheck.ERR_SIZE -> getString(R.string.file_err_hash)
        FileTransfers.ERR_PROTOCOL, ReceiveCheck.ERR_SEQUENCE -> getString(R.string.file_err_protocol)
        else -> getString(R.string.file_err_refused, error)
    }

    private val fileListener = object : FileTransfers.Listener {
        override fun onProgress(up: FileTransfers.Row?, down: FileTransfers.Row?) {
            if (!isDestroyed) transferDock.showProgress(up, down)
        }

        override fun onUploadBatchDone(purpose: String, saved: Int, failed: Int) {
            if (isDestroyed || saved == 0) return
            val text = when {
                failed > 0 -> resources.getQuantityString(R.plurals.file_uploaded_partly, saved + failed, saved, saved + failed)
                purpose == FileWire.PURPOSE_PASTE -> getString(R.string.file_pasted)
                else -> resources.getQuantityString(R.plurals.file_uploaded, saved, saved)
            }
            transferDock.showNotice(text)
        }

        override fun onDownloaded(name: String, uri: Uri, mime: String) {
            DownloadNotifier.notify(this@StreamActivity, name, uri, mime)
            if (!isDestroyed) transferDock.showNotice(getString(R.string.file_saved_as, name), uri, mime)
        }

        override fun onFailed(direction: FileTransfers.Direction, name: String, error: String) {
            if (isDestroyed || error == FileTransfers.ERR_CANCELLED) return
            val res = if (direction == FileTransfers.Direction.UP) R.string.file_failed_up else R.string.file_failed_down
            transferDock.showNotice(getString(res, name, fileError(error)))
        }
    }

    private fun onStatusTapped() {
        if (lastStatus?.state == Session.State.ERROR || session == null) {
            stopSession(); startSession()
        }
    }

    private fun dp(value: Int) = (value * resources.displayMetrics.density).roundToInt()

    /** The status line's look, shared by the frozen-frame "Reconnecting…" label. */
    private fun styleStatusPill(view: TextView) {
        view.setTextColor(Color.WHITE)
        view.textSize = 12f
        view.setPadding(dp(10), dp(4), dp(10), dp(4))
        view.setBackgroundColor(0x99000000.toInt())
    }

    companion object {
        private const val TAG = "BubblePlayStream"
        private const val STATUS_PENDING = 0xFFFFE08A.toInt()
        /** About 35 % black over the frozen frame. */
        private const val FROZEN_DIM = 0x59000000
        private const val FROZEN_FADE_MS = 150L
        private const val BACKGROUND_GRACE_MS = 30_000L
        private const val ESC_HOLD_MS = 1_000L
        private const val SCROLL_DP_PER_NOTCH = 40f
        private const val MONITOR_REFRESH_MS = 1_500L
        private const val REQUEST_ATTACH = 41
        private const val REQUEST_NOTIFY = 42
    }
}
