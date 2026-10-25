package de.bubblebuffer.bubbleplay.ui

import android.app.Activity
import android.app.AlertDialog
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
        frame.addView(ScrollView(this).apply { addView(column) })
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
        column.addView(views.button(R.string.main_unlock_action, KeyViews.Style.PRIMARY) { openUnlock() })
        if (!unlockPrompted) {
            unlockPrompted = true
            openUnlock()
        }
    }

    /** This phone's ID, the PCs it has paired, and the actions that lead on from them. */
    private fun showPcs(deviceId: String) {
        column.addView(views.title(R.string.pcs_title))
        column.addView(views.fieldLabel(R.string.pcs_this_device))
        column.addView(views.mono(deviceId))
        column.addView(views.button(R.string.dk_open, KeyViews.Style.SECONDARY) { openKeys(null) })
        column.addView(views.button(R.string.settings_open, KeyViews.Style.SECONDARY) { openSettings() })

        val list = pcs.list()
        if (list.isEmpty()) {
            column.addView(views.body(R.string.pcs_empty).apply { setPadding(0, views.dp(16), 0, 0) })
        } else {
            list.forEach { column.addView(pcRow(it)) }
        }
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
            setPadding(views.dp(14), views.dp(10), views.dp(4), views.dp(10))
            background = views.panel()
            isClickable = true
            isFocusable = true
            layoutParams = LinearLayout.LayoutParams(
                ViewGroup.LayoutParams.MATCH_PARENT,
                ViewGroup.LayoutParams.WRAP_CONTENT,
            ).apply { topMargin = views.dp(10) }
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
        text.addView(views.mono(pc.id, 13f))
        text.addView(TextView(this).apply {
            this.text = Uri.parse(pc.relay).host ?: pc.relay
            setTextSize(TypedValue.COMPLEX_UNIT_SP, 13f)
            setTextColor(KeyPalette.THREAD_STRONG)
        })
        row.addView(text)
        row.addView(ImageButton(this).apply {
            setImageResource(R.drawable.ic_bar_more)
            setBackgroundColor(0)
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

    private fun actions(pc: PcEntry) {
        AlertDialog.Builder(this)
            .setTitle(PairForm.displayName(pc))
            .setItems(
                arrayOf(
                    getString(R.string.pcs_rename),
                    getString(R.string.pcs_details),
                    getString(R.string.pcs_remove),
                ),
            ) { _, which ->
                when (which) {
                    0 -> rename(pc)
                    1 -> details(pc)
                    else -> confirmRemove(pc)
                }
            }
            .setNegativeButton(R.string.dk_cancel, null)
            .show()
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

    private fun confirmRemove(pc: PcEntry) {
        val dialog = AlertDialog.Builder(this)
            .setTitle(R.string.pcs_remove_title)
            .setMessage(getString(R.string.pcs_remove_message, PairForm.displayName(pc)))
            .setPositiveButton(R.string.pcs_remove_action) { _, _ ->
                pcs.remove(pc.id)
                refresh()
            }
            .setNegativeButton(R.string.dk_cancel, null)
            .show()
        dialog.getButton(AlertDialog.BUTTON_POSITIVE).setTextColor(KeyPalette.DANGER)
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
