package de.bubblebuffer.bubbleplay.ui

import android.app.Activity
import android.content.Intent
import android.graphics.Color
import android.os.Bundle
import android.text.InputType
import android.view.Gravity
import android.view.ViewGroup
import android.webkit.CookieManager
import android.widget.Button
import android.widget.EditText
import android.widget.FrameLayout
import android.widget.LinearLayout
import android.widget.RadioButton
import android.widget.RadioGroup
import android.widget.ScrollView
import android.widget.TextView
import de.bubblebuffer.bubbleplay.R
import de.bubblebuffer.bubbleplay.net.ServerConfig
import de.bubblebuffer.bubbleplay.net.Session

/** Launcher: server settings, then sign-in (WebView gate) or straight to the stream. */
class MainActivity : Activity(), SettingsView.Host {
    private lateinit var prefs: Prefs
    private lateinit var server: EditText
    private lateinit var labRun: EditText
    private lateinit var codec: RadioGroup
    private lateinit var message: TextView
    private lateinit var frame: FrameLayout
    private var settingsView: SettingsView? = null

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        prefs = Prefs(this)
        val pad = dp(20)
        val column = LinearLayout(this).apply {
            orientation = LinearLayout.VERTICAL
            setPadding(pad, pad, pad, pad)
        }
        column.addView(TextView(this).apply {
            setText(R.string.app_name)
            textSize = 28f
            setTextColor(Color.WHITE)
        })
        column.addView(TextView(this).apply {
            setText(R.string.main_intro)
            setTextColor(0xFFB8C0CC.toInt())
            setPadding(0, dp(4), 0, dp(16))
        })
        column.addView(label(R.string.server_label))
        server = EditText(this).apply {
            setText(prefs.server)
            inputType = InputType.TYPE_CLASS_TEXT or InputType.TYPE_TEXT_VARIATION_URI
            setSingleLine()
        }
        column.addView(server)
        column.addView(label(R.string.lab_label))
        labRun = EditText(this).apply {
            setText(prefs.labRun)
            setHint(R.string.lab_hint)
            inputType = InputType.TYPE_CLASS_TEXT or InputType.TYPE_TEXT_FLAG_NO_SUGGESTIONS
            setSingleLine()
        }
        column.addView(labRun)
        column.addView(label(R.string.codec_label))
        codec = RadioGroup(this).apply { orientation = RadioGroup.HORIZONTAL }
        listOf("auto" to R.string.codec_auto, "hevc" to R.string.codec_hevc, "h264" to R.string.codec_h264).forEachIndexed { i, (value, text) ->
            codec.addView(RadioButton(this).apply {
                id = 100 + i
                setText(text)
                tag = value
                isChecked = prefs.codec == value
            })
        }
        column.addView(codec)
        message = TextView(this).apply {
            setTextColor(0xFFFFB4A8.toInt())
            setPadding(0, dp(12), 0, 0)
        }
        column.addView(message)
        column.addView(Button(this).apply {
            setText(R.string.connect)
            setOnClickListener { connect(forceLogin = false) }
        }, LinearLayout.LayoutParams(ViewGroup.LayoutParams.MATCH_PARENT, ViewGroup.LayoutParams.WRAP_CONTENT).apply { topMargin = dp(16) })
        column.addView(Button(this).apply {
            setText(R.string.sign_in_again)
            setOnClickListener { connect(forceLogin = true) }
        })
        column.addView(Button(this).apply {
            setText(R.string.settings_open)
            setOnClickListener { openSettings() }
        })
        column.addView(Button(this).apply {
            setText(R.string.forget_browser)
            setOnClickListener {
                CookieManager.getInstance().removeAllCookies(null)
                CookieManager.getInstance().flush()
                message.setText(R.string.forgot_browser)
            }
        })
        frame = FrameLayout(this).apply {
            setBackgroundColor(0xFF10141A.toInt())
            fitsSystemWindows = true // API 35 draws edge-to-edge; keep the form and settings clear of the bars.
        }
        frame.addView(ScrollView(this).apply { addView(column) })
        setContentView(frame)
        showIntentMessage(intent)
    }

    private fun showIntentMessage(intent: Intent?) {
        when {
            intent?.getBooleanExtra(EXTRA_SIGNED_OUT, false) == true -> message.setText(R.string.settings_signed_out_notice)
            intent?.getBooleanExtra(EXTRA_AUTH_EXPIRED, false) == true -> message.setText(R.string.auth_expired)
        }
    }

    // ---- settings view -------------------------------------------------------------------------
    private fun openSettings() {
        if (settingsView != null) return
        // Unsaved edits to the server field are kept as the starting value.
        ServerConfig.parseServer(server.text.toString())?.let { prefs.server = it.toString() }
        val view = SettingsView(this, prefs, this)
        settingsView = view
        frame.addView(view, FrameLayout.LayoutParams(ViewGroup.LayoutParams.MATCH_PARENT, ViewGroup.LayoutParams.MATCH_PARENT))
    }

    override fun closeSettings() {
        val view = settingsView ?: return
        (getSystemService(INPUT_METHOD_SERVICE) as android.view.inputmethod.InputMethodManager).hideSoftInputFromWindow(view.windowToken, 0)
        frame.removeView(view)
        settingsView = null
        // The form shows what Settings saved.
        server.setText(prefs.server)
        codec.findViewWithTag<RadioButton>(prefs.codec)?.isChecked = true
    }

    override val settingsConfig: ServerConfig? get() = prefs.serverConfig()
    override val inSession: Boolean get() = false
    override val settingsMonitors: List<Session.Monitor> get() = emptyList()
    override fun diagnostics(): Session.Diagnostics? = null
    override fun probeRoundTrip() {}
    override fun onSettingChanged(setting: SettingsView.Setting) {}

    override fun signOut() {
        val config = prefs.serverConfig() ?: return
        Prefs.signOut(config) {
            message.setText(R.string.settings_signed_out_notice)
            settingsView?.connectionChanged()
        }
    }

    @Deprecated("Deprecated in Java")
    override fun onBackPressed() {
        if (settingsView != null) closeSettings() else {
            @Suppress("DEPRECATION")
            super.onBackPressed()
        }
    }

    private fun connect(forceLogin: Boolean) {
        val base = ServerConfig.parseServer(server.text.toString())
        if (base == null) { message.setText(R.string.bad_server); return }
        val run = labRun.text.toString().trim()
        if (run.isNotEmpty() && !ServerConfig.validLabRun(run)) { message.setText(R.string.bad_lab_run); return }
        prefs.server = base.toString()
        prefs.labRun = run
        prefs.codec = codec.findViewById<RadioButton>(codec.checkedRadioButtonId)?.tag as? String ?: "auto"
        val config = prefs.serverConfig() ?: return
        val signedIn = ServerConfig.hasSessionCookies(Prefs.cookieHeader(config))
        val next = if (config.isLab || (signedIn && !forceLogin)) StreamActivity::class.java else LoginActivity::class.java
        startActivity(PendingShares.attach(Intent(this, next).putExtra(LoginActivity.EXTRA_FORCE, forceLogin), PendingShares.read(intent)))
        PendingShares.consume(intent)
    }

    override fun onNewIntent(intent: Intent) {
        super.onNewIntent(intent)
        setIntent(intent) // shares from the share sheet, kept until Connect
        showIntentMessage(intent)
    }

    private fun label(text: Int) = TextView(this).apply {
        setText(text)
        setTextColor(0xFFDDE3EA.toInt())
        gravity = Gravity.START
        setPadding(0, dp(12), 0, 0)
    }

    private fun dp(value: Int) = (value * resources.displayMetrics.density).toInt()

    companion object {
        const val EXTRA_AUTH_EXPIRED = "authExpired"
        const val EXTRA_SIGNED_OUT = "signedOut"
    }
}
