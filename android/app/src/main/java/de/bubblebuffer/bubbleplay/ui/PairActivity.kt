package de.bubblebuffer.bubbleplay.ui

import android.app.Activity
import android.content.Intent
import android.graphics.Typeface
import android.os.Bundle
import android.text.InputType
import android.view.View
import android.widget.EditText
import android.widget.LinearLayout
import android.widget.ScrollView
import android.widget.TextView
import de.bubblebuffer.bubbleplay.R
import de.bubblebuffer.bubbleplay.auth.PairLink
import de.bubblebuffer.bubbleplay.auth.PairingCode
import de.bubblebuffer.bubbleplay.keys.DeviceKeyStore
import de.bubblebuffer.bubbleplay.keys.UnlockedKey
import de.bubblebuffer.bubbleplay.net.PairingClient
import de.bubblebuffer.bubbleplay.net.PairingFailure
import de.bubblebuffer.bubbleplay.net.PairingListener
import de.bubblebuffer.bubbleplay.net.PairingStage
import de.bubblebuffer.bubbleplay.pcs.PcEntry
import de.bubblebuffer.bubbleplay.pcs.PcStore
import java.util.concurrent.Executors

/**
 * Pairs this phone with a PC through the relay: the user types the relay, PC ID and pairing code
 * (or opens a `bubbleplay://pair` link, which only fills the form), then presses Pair. Needs the
 * unlocked device key, since the pairing proves it. Progress comes from [PairingListener]; a
 * finished pairing is stored in [PcStore].
 */
class PairActivity : Activity() {
    private lateinit var views: KeyViews
    private lateinit var pcs: PcStore
    private lateinit var keyCard: LinearLayout
    private lateinit var keyBody: TextView
    private lateinit var keyAction: TextView
    private lateinit var form: LinearLayout
    private lateinit var relayField: EditText
    private lateinit var pcIdField: EditText
    private lateinit var codeField: EditText
    private lateinit var nameField: EditText
    private lateinit var status: TextView
    private lateinit var pairButton: TextView
    private lateinit var progress: LinearLayout
    private lateinit var stageLine: TextView
    private lateinit var numberLine: TextView
    private lateinit var paired: LinearLayout
    private lateinit var pairedLine: TextView
    private val worker = Executors.newSingleThreadExecutor()

    /** The attempt in flight, if any. Null once it has ended, been cancelled or been left. */
    private var client: PairingClient? = null

    /** The PC that was just paired, for the Connect button. */
    private var pairedPc: PcEntry? = null

    /** Where the key card's button leads: the device key screen, or the unlock screen. */
    private var keyStart: String? = null

    /** The unlock prompt opens by itself once per activity, so cancelling it does not loop. */
    private var unlockPrompted = false

    private val listener = object : PairingListener {
        override fun onStage(stage: PairingStage) {
            stageLine.setText(
                when (stage) {
                    PairingStage.Connecting -> R.string.pair_connecting
                    PairingStage.Proving -> R.string.pair_proving
                    PairingStage.WaitingForPc -> R.string.pair_waiting
                },
            )
        }

        override fun onNumber(display: String) {
            numberLine.text = display
            numberLine.visibility = View.VISIBLE
        }

        override fun onPaired(pc: PcEntry) {
            client = null
            pcs.put(pc)
            showPaired(pc)
        }

        override fun onDenied() {
            client = null
            showForm(R.string.pair_denied, error = true)
        }

        override fun onFailed(reason: PairingFailure) {
            client = null
            showForm(failureText(reason), error = true)
        }
    }

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        views = KeyViews(this)
        pcs = PcStore.from(this)
        val column = LinearLayout(this).apply {
            orientation = LinearLayout.VERTICAL
            setPadding(views.dp(20), views.dp(20), views.dp(20), views.dp(20))
        }
        column.addView(views.title(R.string.pair_title))
        column.addView(views.body(R.string.pair_intro))

        keyCard = LinearLayout(this).apply {
            orientation = LinearLayout.VERTICAL
            setPadding(views.dp(16), views.dp(14), views.dp(16), views.dp(14))
            background = views.panel()
            visibility = View.GONE
        }
        keyBody = views.body("")
        keyAction = views.button(R.string.dk_open, KeyViews.Style.SECONDARY) { openKeys(keyStart) }
        keyCard.addView(keyBody)
        keyCard.addView(keyAction)
        column.addView(keyCard)

        form = LinearLayout(this).apply { orientation = LinearLayout.VERTICAL }
        relayField = views.field(R.string.pair_relay_hint).apply {
            inputType = InputType.TYPE_CLASS_TEXT or InputType.TYPE_TEXT_VARIATION_URI
        }
        pcIdField = monoField(R.string.pair_pcid_hint)
        codeField = monoField(R.string.pair_code_hint)
        nameField = views.field(R.string.pair_name_hint)
        form.addView(views.fieldLabel(R.string.pair_relay))
        form.addView(relayField)
        form.addView(views.fieldLabel(R.string.pair_pcid))
        form.addView(pcIdField)
        form.addView(views.fieldLabel(R.string.pair_code))
        form.addView(codeField)
        form.addView(views.fieldLabel(R.string.pair_name))
        form.addView(nameField)
        status = views.status()
        status.visibility = View.GONE
        form.addView(status)
        pairButton = views.button(R.string.pair_action, KeyViews.Style.PRIMARY) { pair() }
        form.addView(pairButton)
        column.addView(form)

        progress = LinearLayout(this).apply {
            orientation = LinearLayout.VERTICAL
            visibility = View.GONE
        }
        stageLine = views.body("").apply { accessibilityLiveRegion = View.ACCESSIBILITY_LIVE_REGION_POLITE }
        numberLine = views.mono("", 36f).apply { visibility = View.GONE }
        progress.addView(stageLine)
        progress.addView(numberLine)
        progress.addView(views.button(R.string.pair_cancel, KeyViews.Style.SECONDARY) { cancelPairing() })
        column.addView(progress)

        paired = LinearLayout(this).apply {
            orientation = LinearLayout.VERTICAL
            visibility = View.GONE
        }
        pairedLine = views.body("").apply { accessibilityLiveRegion = View.ACCESSIBILITY_LIVE_REGION_POLITE }
        paired.addView(pairedLine)
        paired.addView(views.button(R.string.pair_connect, KeyViews.Style.PRIMARY) { connectPaired() })
        paired.addView(views.button(R.string.pair_done, KeyViews.Style.SECONDARY) { finish() })
        column.addView(paired)

        setContentView(ScrollView(this).apply {
            setBackgroundColor(KeyPalette.CANVAS)
            fitsSystemWindows = true
            addView(column)
        })

        // The PC list's relay is the usual one; a link from the PC overrides it.
        relayField.setText(PairForm.suggestedRelay(pcs.list()).orEmpty())
        applyLink(intent)
    }

    override fun onNewIntent(intent: Intent) {
        super.onNewIntent(intent)
        setIntent(intent)
        applyLink(intent) // fills the form only; the user presses Pair
    }

    override fun onResume() {
        super.onResume()
        checkKey()
    }

    override fun onDestroy() {
        client?.cancel()
        client = null
        worker.shutdown()
        super.onDestroy()
    }

    /** Reads the stored key off the UI thread, then shows the key card when pairing needs an unlock. */
    private fun checkKey() {
        val store = DeviceKeyStore(this)
        worker.execute {
            val stored = DeviceKeyLogic.read(store)
            runOnUiThread {
                if (!isDestroyed) showKeyState(stored)
            }
        }
    }

    private fun showKeyState(stored: StoredKey) {
        val screen = DeviceKeyLogic.startScreen(stored, unlocked = UnlockedKey.get() != null)
        val locked = screen != StartScreen.PcList
        keyCard.visibility = if (locked) View.VISIBLE else View.GONE
        pairButton.isEnabled = !locked
        when (screen) {
            StartScreen.Setup -> {
                keyBody.setText(R.string.pair_needs_key)
                keyAction.setText(R.string.dk_open)
                keyStart = null
            }
            StartScreen.Unlock -> {
                keyBody.setText(R.string.pair_needs_unlock)
                keyAction.setText(R.string.main_unlock_action)
                keyStart = DeviceKeyActivity.START_UNLOCK
                if (!unlockPrompted) {
                    unlockPrompted = true
                    openKeys(DeviceKeyActivity.START_UNLOCK)
                }
            }
            StartScreen.PcList -> Unit
        }
    }

    private fun openKeys(start: String?) {
        startActivity(DeviceKeyActivity.intent(this, start))
    }

    /** Fills the form from a `bubbleplay://pair` link. A link that does not parse says so and changes nothing. */
    private fun applyLink(intent: Intent?) {
        if (intent?.action != Intent.ACTION_VIEW) return
        val uri = intent.data ?: return
        val link = PairLink.parse(uri.toString())
        if (link == null) {
            views.tell(status, R.string.pair_bad_link, error = true)
            status.visibility = View.VISIBLE
            return
        }
        relayField.setText(link.relay)
        pcIdField.setText(link.pcId)
        codeField.setText(PairingCode.display(link.code))
    }

    private fun pair() {
        val relay = PairForm.relayOrigin(relayField.text.toString())
        val pcId = PairForm.pcId(pcIdField.text.toString())
        val code = PairingCode.normalise(codeField.text.toString())
        when {
            relay == null -> showForm(R.string.pair_err_relay, error = true)
            pcId == null -> showForm(R.string.pair_err_pcid, error = true)
            code == null -> showForm(R.string.pair_err_code, error = true)
            else -> {
                val key = UnlockedKey.get()
                if (key == null) {
                    checkKey()
                    return
                }
                val name = PairForm.name(nameField.text.toString(), pcId)
                client = PairingClient(key, PairLink(relay, pcId, code), name, listener).also { it.start() }
                showProgress()
            }
        }
    }

    private fun cancelPairing() {
        client?.cancel()
        client = null
        showForm(R.string.pair_cancelled, error = false)
    }

    /** The form, with [message] under it when given. A failure keeps the typed fields and offers Try again. */
    private fun showForm(message: Int? = null, error: Boolean = false) {
        progress.visibility = View.GONE
        paired.visibility = View.GONE
        form.visibility = View.VISIBLE
        pairButton.setText(if (error) R.string.pair_retry else R.string.pair_action)
        if (message == null) {
            status.visibility = View.GONE
        } else {
            views.tell(status, message, error)
        }
    }

    private fun showProgress() {
        form.visibility = View.GONE
        paired.visibility = View.GONE
        progress.visibility = View.VISIBLE
        numberLine.visibility = View.GONE
        stageLine.setText(R.string.pair_connecting)
    }

    private fun showPaired(pc: PcEntry) {
        pairedPc = pc
        progress.visibility = View.GONE
        form.visibility = View.GONE
        paired.visibility = View.VISIBLE
        pairedLine.text = getString(R.string.pair_paired, PairForm.displayName(pc))
    }

    private fun connectPaired() {
        val pc = pairedPc ?: return
        startActivity(Intent(this, StreamActivity::class.java).putExtra(StreamActivity.EXTRA_PC_ID, pc.id))
        finish()
    }

    private fun failureText(reason: PairingFailure): Int = when (reason) {
        PairingFailure.Refused -> R.string.pair_fail_refused
        PairingFailure.ProofMismatch -> R.string.pair_fail_proof
        PairingFailure.PcKeyInvalid -> R.string.pair_fail_key
        PairingFailure.ConnectionLost -> R.string.pair_fail_lost
        PairingFailure.Timeout -> R.string.pair_fail_timeout
    }

    private fun monoField(hint: Int): EditText = views.field(hint).apply {
        typeface = Typeface.MONOSPACE
        inputType = InputType.TYPE_CLASS_TEXT or InputType.TYPE_TEXT_FLAG_NO_SUGGESTIONS
    }
}
