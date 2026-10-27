package de.bubblebuffer.bubbleplay.ui

import android.app.Activity
import android.app.AlertDialog
import android.app.Dialog
import android.content.Intent
import android.net.Uri
import android.os.Bundle
import android.text.format.DateFormat
import android.util.TypedValue
import android.view.Gravity
import android.view.ViewGroup
import android.view.inputmethod.InputMethodManager
import android.widget.FrameLayout
import android.widget.ImageButton
import android.widget.LinearLayout
import android.widget.ScrollView
import android.widget.TextView
import de.bubblebuffer.bubbleplay.R
import de.bubblebuffer.bubbleplay.keys.DeviceKeyStore
import de.bubblebuffer.bubbleplay.keys.UnlockedKey
import de.bubblebuffer.bubbleplay.net.ServerConfig
import de.bubblebuffer.bubbleplay.net.Session
import de.bubblebuffer.bubbleplay.pcs.PcEntry
import de.bubblebuffer.bubbleplay.pcs.PcStore
import java.util.Date
import java.util.concurrent.Executors

/**
 * Launcher. The PC list is shown only with an unlocked device key: without a key the setup card
 * shows (create or import), and with a locked key the unlock screen opens [DeviceKeyActivity] in
 * unlock mode. Tapping a PC connects to it; pairing a new one starts [PairActivity].
 */
class MainActivity : Activity(), SettingsView.Host {
    private lateinit var prefs: Prefs
    private lateinit var pcs: PcStore
    private lateinit var views: KeyViews
    private lateinit var column: LinearLayout
    private lateinit var frame: FrameLayout
    private val worker = Executors.newSingleThreadExecutor()
    private var settingsView: SettingsView? = null

    /** The unlock prompt opens by itself once per activity, so cancelling it does not loop. */
    private var unlockPrompted = false

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        prefs = Prefs(this)
        pcs = PcStore.from(this)
        views = KeyViews(this)
        column = LinearLayout(this).apply {
            orientation = LinearLayout.VERTICAL
            setPadding(views.dp(20), views.dp(20), views.dp(20), views.dp(20))
        }
        frame = FrameLayout(this).apply {
            setBackgroundColor(KeyPalette.CANVAS)
            fitsSystemWindows = true // API 35 draws edge-to-edge; keep the list and settings clear of the bars.
        }
        frame.addView(ScrollView(this).apply {
            isFillViewport = true
            addView(column)
        })
        setContentView(frame)
    }

    override fun onResume() {
        super.onResume()
        refresh()
    }

    override fun onNewIntent(intent: Intent) {
        super.onNewIntent(intent)
        setIntent(intent) // shares from the share sheet, kept until a PC is chosen
    }

    override fun onDestroy() {
        worker.shutdown()
        super.onDestroy()
    }

    /** Reads the stored key off the UI thread, then draws the screen the key state allows. */
    private fun refresh() {
        val store = DeviceKeyStore(this)
        worker.execute {
            val stored = DeviceKeyLogic.read(store)
            runOnUiThread {
                if (!isDestroyed) show(stored)
            }
        }
    }

    private fun show(stored: StoredKey) {
        column.removeAllViews()
        when (DeviceKeyLogic.startScreen(stored, unlocked = UnlockedKey.get() != null)) {
            StartScreen.Setup -> showSetup(unreadable = stored is StoredKey.Unreadable)
            StartScreen.Unlock -> showUnlock()
            StartScreen.PcList -> showPcs((stored as StoredKey.Present).file.id)
        }
    }

    // ---- start screens ----------------------------------------------------------------------------

    private fun showSetup(unreadable: Boolean) {
        column.addView(views.title(R.string.app_name))
        if (unreadable) {
            column.addView(views.body(R.string.main_setup_unreadable))
            column.addView(views.button(R.string.dk_import, KeyViews.Style.PRIMARY) { openKeys(DeviceKeyActivity.START_IMPORT) })
            column.addView(views.button(R.string.dk_open, KeyViews.Style.SECONDARY) { openKeys(null) })
        } else {
            column.addView(views.body(R.string.main_setup_body))
            column.addView(views.button(R.string.dk_create, KeyViews.Style.PRIMARY) { openKeys(DeviceKeyActivity.START_CREATE) })
            column.addView(views.button(R.string.dk_import, KeyViews.Style.SECONDARY) { openKeys(DeviceKeyActivity.START_IMPORT) })
        }
    }

    private fun showUnlock() {
        column.addView(views.title(R.string.main_unlock_title))
        column.addView(views.body(R.string.main_unlock_body))
        column.addView(views.spacer())
        column.addView(views.button(R.string.main_unlock_action, KeyViews.Style.PRIMARY) { openUnlock() })
        if (!unlockPrompted) {
            unlockPrompted = true
            openUnlock()
        }
    }

    /** This phone's ID, the PCs it has paired, and the actions that lead on from them. */
    private fun showPcs(deviceId: String) {
        column.addView(views.title(R.string.pcs_title))
        column.addView(LinearLayout(this).apply {
            orientation = LinearLayout.HORIZONTAL
            gravity = Gravity.CENTER_VERTICAL or Gravity.START
            setPadding(0, 0, 0, views.dp(16))
            addView(TextView(this@MainActivity).apply {
                setText(R.string.pcs_this_device)
                setTextSize(TypedValue.COMPLEX_UNIT_SP, 14f)
                setTextColor(KeyPalette.CREAM)
                setPadding(0, 0, views.dp(8), 0)
            })
            addView(views.mono(deviceId, 14f).apply { setPadding(0, 0, 0, 0) })
        })

        val list = pcs.list()
        if (list.isEmpty()) {
            column.addView(views.body(R.string.pcs_empty).apply { setPadding(0, views.dp(8), 0, 0) })
        } else {
            column.addView(views.rule())
            list.forEach {
                column.addView(pcRow(it))
                column.addView(views.rule())
            }
        }
        column.addView(views.spacer())
        column.addView(views.button(R.string.dk_open, KeyViews.Style.SECONDARY) { openKeys(null) })
        column.addView(views.button(R.string.settings_open, KeyViews.Style.SECONDARY) { openSettings() })
        column.addView(views.button(R.string.pcs_pair, KeyViews.Style.PRIMARY) {
            startActivity(Intent(this, PairActivity::class.java))
        })
    }

    private fun openKeys(start: String?) {
        startActivity(DeviceKeyActivity.intent(this, start))
    }

    private fun openUnlock() {
        openKeys(DeviceKeyActivity.START_UNLOCK)
    }

    /** One PC: tap connects, long-press or the overflow button opens its actions. */
    private fun pcRow(pc: PcEntry): LinearLayout {
        val row = LinearLayout(this).apply {
            orientation = LinearLayout.HORIZONTAL
            gravity = Gravity.CENTER_VERTICAL
            minimumHeight = views.dp(66)
            setPadding(0, views.dp(10), 0, views.dp(10))
            background = views.rowBackground()
            isClickable = true
            isFocusable = true
            layoutParams = LinearLayout.LayoutParams(
                ViewGroup.LayoutParams.MATCH_PARENT,
                ViewGroup.LayoutParams.WRAP_CONTENT,
            )
            setOnClickListener { connect(pc) }
            setOnLongClickListener {
                actions(pc)
                true
            }
        }
        val text = LinearLayout(this).apply {
            orientation = LinearLayout.VERTICAL
            layoutParams = LinearLayout.LayoutParams(0, ViewGroup.LayoutParams.WRAP_CONTENT, 1f)
        }
        text.addView(TextView(this).apply {
            this.text = PairForm.displayName(pc)
            setTextSize(TypedValue.COMPLEX_UNIT_SP, 17f)
            setTextColor(KeyPalette.MILK)
        })
        text.addView(views.mono(pc.id, 13f).apply {
            setTextColor(KeyPalette.CREAM)
            setPadding(0, views.dp(2), 0, 0)
        })
        text.addView(TextView(this).apply {
            this.text = Uri.parse(pc.relay).host ?: pc.relay
            setTextSize(TypedValue.COMPLEX_UNIT_SP, 13f)
            setTextColor(KeyPalette.CREAM)
            setPadding(0, views.dp(1), 0, 0)
        })
        row.addView(text)
        row.addView(ImageButton(this).apply {
            setImageResource(R.drawable.ic_bar_more)
            imageTintList = android.content.res.ColorStateList.valueOf(KeyPalette.CREAM)
            background = views.rowBackground()
            contentDescription = getString(R.string.pcs_actions)
            setOnClickListener { actions(pc) }
            layoutParams = LinearLayout.LayoutParams(views.dp(48), views.dp(48))
        })
        return row
    }

    // ---- PC actions -------------------------------------------------------------------------------

    private fun connect(pc: PcEntry) {
        startActivity(
            PendingShares.attach(
                Intent(this, StreamActivity::class.java).putExtra(StreamActivity.EXTRA_PC_ID, pc.id),
                PendingShares.read(intent),
            ),
        )
        PendingShares.consume(intent)
    }

    /** The PC's actions as a bottom sheet: its name and ID on top, Remove set apart, Cancel last. */
    private fun actions(pc: PcEntry) {
        val sheet = Dialog(this)
        sheet.requestWindowFeature(android.view.Window.FEATURE_NO_TITLE)
        val body = LinearLayout(this).apply {
            orientation = LinearLayout.VERTICAL
            background = views.sheetBackground()
            setPadding(views.dp(20), views.dp(16), views.dp(20), views.dp(20))
        }
        body.addView(TextView(this).apply {
            text = PairForm.displayName(pc)
            setTextSize(TypedValue.COMPLEX_UNIT_SP, 18f)
            setTypeface(typeface, android.graphics.Typeface.BOLD)
            setTextColor(KeyPalette.MILK)
            isAccessibilityHeading = true
        })
        body.addView(views.mono(pc.id, 13f).apply {
            setTextColor(KeyPalette.CREAM)
            setPadding(0, views.dp(2), 0, views.dp(8))
        })
        fun item(label: Int, color: Int, gap: Boolean, act: () -> Unit) {
            body.addView(views.rule(if (gap) KeyPalette.THREAD_STRONG else KeyPalette.THREAD).apply {
                (layoutParams as LinearLayout.LayoutParams).topMargin = if (gap) views.dp(14) else 0
            })
            body.addView(TextView(this).apply {
                setText(label)
                setTextSize(TypedValue.COMPLEX_UNIT_SP, 17f)
                setTextColor(color)
                gravity = Gravity.CENTER_VERTICAL
                minHeight = views.dp(52)
                background = views.rowBackground()
                isClickable = true
                isFocusable = true
                setOnClickListener {
                    sheet.dismiss()
                    act()
                }
            })
        }
        item(R.string.pcs_rename, KeyPalette.MILK, false) { rename(pc) }
        item(R.string.pcs_details, KeyPalette.MILK, false) { details(pc) }
        item(R.string.pcs_remove, KeyPalette.DANGER, true) { confirmRemove(pc) }
        body.addView(views.button(R.string.dk_cancel, KeyViews.Style.SECONDARY) { sheet.dismiss() }.apply {
            (layoutParams as LinearLayout.LayoutParams).topMargin = views.dp(12)
        })
        sheet.setContentView(body)
        sheet.window?.apply {
            setBackgroundDrawable(android.graphics.drawable.ColorDrawable(0))
            setLayout(ViewGroup.LayoutParams.MATCH_PARENT, ViewGroup.LayoutParams.WRAP_CONTENT)
            setGravity(Gravity.BOTTOM)
            setDimAmount(0.72f)
        }
        sheet.show()
    }

    private fun rename(pc: PcEntry) {
        val field = views.field(R.string.pcs_rename_hint).apply { setText(pc.name) }
        val container = FrameLayout(this).apply {
            setPadding(views.dp(20), views.dp(8), views.dp(20), 0)
            addView(field)
        }
        AlertDialog.Builder(this)
            .setTitle(R.string.pcs_rename_title)
            .setView(container)
            .setPositiveButton(R.string.pcs_rename_action) { _, _ ->
                pcs.rename(pc.id, field.text.toString().trim())
                refresh()
            }
            .setNegativeButton(R.string.dk_cancel, null)
            .show()
    }

    private fun details(pc: PcEntry) {
        val body = LinearLayout(this).apply {
            orientation = LinearLayout.VERTICAL
            setPadding(views.dp(20), views.dp(8), views.dp(20), 0)
        }
        body.addView(views.fieldLabel(R.string.pcs_details_name))
        body.addView(views.body(PairForm.displayName(pc)))
        body.addView(views.fieldLabel(R.string.pcs_details_id))
        body.addView(views.mono(pc.id))
        body.addView(views.fieldLabel(R.string.pcs_details_relay))
        body.addView(views.body(pc.relay))
        body.addView(views.fieldLabel(R.string.pcs_details_key))
        body.addView(views.mono(pc.publicKey, 13f))
        body.addView(views.fieldLabel(R.string.pcs_details_paired))
        body.addView(views.body(DateFormat.getMediumDateFormat(this).format(Date(pc.paired))))
        AlertDialog.Builder(this)
            .setTitle(R.string.pcs_details_title)
            .setView(body)
            .setPositiveButton(R.string.pcs_details_close, null)
            .show()
    }

    /** Cancel and Remove side by side; Remove is the one in danger colour and the message names the PC. */
    private fun confirmRemove(pc: PcEntry) {
        val dialog = Dialog(this)
        dialog.requestWindowFeature(android.view.Window.FEATURE_NO_TITLE)
        val body = LinearLayout(this).apply {
            orientation = LinearLayout.VERTICAL
            background = views.panel()
            setPadding(views.dp(20), views.dp(20), views.dp(20), views.dp(20))
        }
        body.addView(TextView(this).apply {
            setText(R.string.pcs_remove_title)
            setTextSize(TypedValue.COMPLEX_UNIT_SP, 20f)
            setTypeface(typeface, android.graphics.Typeface.BOLD)
            setTextColor(KeyPalette.MILK)
            setPadding(0, 0, 0, views.dp(8))
            isAccessibilityHeading = true
        })
        body.addView(views.body(getString(R.string.pcs_remove_message, PairForm.displayName(pc))))
        val buttons = LinearLayout(this).apply { orientation = LinearLayout.HORIZONTAL }
        fun half(label: Int, style: KeyViews.Style, start: Int, act: () -> Unit) = views.button(label, style) {
            dialog.dismiss()
            act()
        }.apply {
            layoutParams = LinearLayout.LayoutParams(0, ViewGroup.LayoutParams.WRAP_CONTENT, 1f).apply {
                topMargin = views.dp(14)
                marginStart = start
            }
        }
        buttons.addView(half(R.string.dk_cancel, KeyViews.Style.SECONDARY, 0) {})
        buttons.addView(half(R.string.pcs_remove_action, KeyViews.Style.DANGER, views.dp(10)) {
            pcs.remove(pc.id)
            refresh()
        })
        body.addView(buttons)
        dialog.setContentView(body)
        dialog.window?.apply {
            setBackgroundDrawable(android.graphics.drawable.ColorDrawable(0))
            setLayout(resources.displayMetrics.widthPixels - views.dp(40), ViewGroup.LayoutParams.WRAP_CONTENT)
            setDimAmount(0.72f)
        }
        dialog.show()
    }

    // ---- settings view -------------------------------------------------------------------------

    private fun openSettings() {
        if (settingsView != null) return
        val view = SettingsView(this, prefs, this)
        settingsView = view
        frame.addView(view, FrameLayout.LayoutParams(ViewGroup.LayoutParams.MATCH_PARENT, ViewGroup.LayoutParams.MATCH_PARENT))
    }

    override fun closeSettings() {
        val view = settingsView ?: return
        (getSystemService(INPUT_METHOD_SERVICE) as InputMethodManager).hideSoftInputFromWindow(view.windowToken, 0)
        frame.removeView(view)
        settingsView = null
    }

    override val settingsConfig: ServerConfig? get() = prefs.serverConfig()
    override val inSession: Boolean get() = false
    override val settingsMonitors: List<Session.Monitor> get() = emptyList()
    override fun diagnostics(): Session.Diagnostics? = null
    override fun probeRoundTrip() {}
    override fun onSettingChanged(setting: SettingsView.Setting) {}

    @Deprecated("Deprecated in Java")
    override fun onBackPressed() {
        if (settingsView != null) closeSettings() else {
            @Suppress("DEPRECATION")
            super.onBackPressed()
        }
    }
}
