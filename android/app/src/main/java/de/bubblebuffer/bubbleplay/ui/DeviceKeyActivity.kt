package de.bubblebuffer.bubbleplay.ui

import android.app.Activity
import android.app.AlertDialog
import android.content.ClipData
import android.content.ClipboardManager
import android.content.Context
import android.content.Intent
import android.graphics.Typeface
import android.net.Uri
import android.os.Build
import android.os.Bundle
import android.util.TypedValue
import android.widget.LinearLayout
import android.widget.ScrollView
import android.widget.TextView
import android.widget.Toast
import de.bubblebuffer.bubbleplay.R
import de.bubblebuffer.bubbleplay.keys.DeviceKey
import de.bubblebuffer.bubbleplay.keys.DeviceKeyException
import de.bubblebuffer.bubbleplay.keys.DeviceKeyStore
import de.bubblebuffer.bubbleplay.keys.KeyFile
import de.bubblebuffer.bubbleplay.keys.KeyFileCodec
import java.io.File
import java.io.IOException
import java.nio.ByteBuffer
import java.nio.charset.CharacterCodingException
import java.nio.charset.CodingErrorAction
import java.util.concurrent.Executors

/**
 * This phone's device key: create, import, export, change the passphrase, or remove it.
 *
 * Phase 1: the key is optional and nothing here blocks connecting. Disk, Keystore and Argon2id
 * work runs on [worker]; passphrases are asked in [PassphraseDialog]. An unlocked key is closed as
 * soon as its one step is done.
 */
class DeviceKeyActivity : Activity() {
    private lateinit var store: DeviceKeyStore
    private lateinit var views: KeyViews
    private lateinit var column: LinearLayout
    private val worker = Executors.newSingleThreadExecutor()

    /** Null until the first read finishes. */
    private var stored: StoredKey? = null

    /** Status line of the screen on show; null only between screens. */
    private var status: TextView? = null

    /** One-time confirmation for the next screen, such as "Key imported". */
    private var notice: Int? = null

    /** Portable text chosen for export, waiting for the save picker. */
    private var pendingExport: String? = null

    /** Which export choices a key offers: keep the file as stored, re-seal it, or drop the passphrase. */
    private enum class Export { KEEP, NEW, NONE }

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        store = DeviceKeyStore(this)
        views = KeyViews(this)
        column = LinearLayout(this).apply {
            orientation = LinearLayout.VERTICAL
            setPadding(views.dp(20), views.dp(20), views.dp(20), views.dp(20))
        }
        setContentView(ScrollView(this).apply {
            setBackgroundColor(KeyPalette.CANVAS)
            fitsSystemWindows = true
            addView(column)
        })
        column.addView(views.body(R.string.dk_loading))
        val start = intent.getStringExtra(EXTRA_START)
        reload {
            render()
            // A new activity from the start screen acts at once; a restored one does not repeat it.
            if (savedInstanceState == null) {
                when {
                    start == START_IMPORT -> pickImport()
                    start == START_CREATE && stored is StoredKey.Missing -> showCreate()
                }
            }
        }
    }

    override fun onDestroy() {
        worker.shutdown()
        super.onDestroy()
    }

    // ---- screens --------------------------------------------------------------------------------

    /** Starts a screen with its title; [build] adds the rest. */
    private fun screen(build: () -> Unit) {
        column.removeAllViews()
        status = null
        column.addView(views.title(R.string.dk_title))
        build()
    }

    /** The status line for the current screen. Takes the pending [notice], if any. */
    private fun statusLine() {
        val line = views.status()
        status = line
        notice?.let { views.tell(line, it, error = false) }
        notice = null
        column.addView(line)
    }

    private fun render() {
        when (val current = stored) {
            null -> screen { column.addView(views.body(R.string.dk_loading)) }
            is StoredKey.Missing -> renderMissing()
            is StoredKey.Present -> renderPresent(current)
            is StoredKey.Unreadable -> renderUnreadable(current)
        }
    }

    private fun renderMissing() = screen {
        column.addView(views.body(R.string.dk_intro_missing))
        column.addView(views.button(R.string.dk_create, KeyViews.Style.PRIMARY) { showCreate() })
        column.addView(views.button(R.string.dk_import, KeyViews.Style.SECONDARY) { pickImport() })
        statusLine()
    }

    private fun renderPresent(key: StoredKey.Present) = screen {
        keySummary(key.file)
        statusLine()
        column.addView(views.button(R.string.dk_export, KeyViews.Style.PRIMARY) { chooseExport(key) })
        column.addView(views.button(R.string.dk_change_passphrase, KeyViews.Style.SECONDARY) {
            reseal(key, askNewPassphrase = true) { writeStored(it, R.string.dk_passphrase_changed) }
        })
        column.addView(views.button(R.string.dk_remove, KeyViews.Style.DANGER) { confirmRemove() })
    }

    private fun renderUnreadable(key: StoredKey.Unreadable) = screen {
        column.addView(views.body(R.string.dk_unreadable_title))
        column.addView(views.body(R.string.dk_unreadable_body))
        column.addView(views.body(DeviceKeyLogic.reasonMessage(key.reason)))
        statusLine()
        column.addView(views.button(R.string.dk_import, KeyViews.Style.PRIMARY) { pickImport() })
        column.addView(views.button(R.string.dk_remove, KeyViews.Style.DANGER) { confirmRemove() })
    }

    private fun showCreate() = screen {
        column.addView(views.body(R.string.dk_create_note))
        val label = views.field(R.string.dk_label_hint).apply { setText(Build.MODEL) }
        val passphrase = views.passwordField(R.string.dk_passphrase_hint)
        val again = views.passwordField(R.string.dk_passphrase_repeat)
        column.addView(views.fieldLabel(R.string.dk_label))
        column.addView(label)
        column.addView(views.fieldLabel(R.string.dk_passphrase))
        column.addView(passphrase)
        column.addView(again)
        column.addView(views.body(R.string.dk_passphrase_note))
        statusLine()
        val create = views.button(R.string.dk_create, KeyViews.Style.PRIMARY) {}
        create.setOnClickListener {
            createKey(label.text.toString(), passphrase.text.toString(), again.text.toString(), create)
        }
        column.addView(create)
        column.addView(views.button(R.string.dk_back, KeyViews.Style.SECONDARY) { render() })
    }

    private fun showCreated(key: StoredKey.Present) = screen {
        column.addView(views.body(R.string.dk_created_title))
        column.addView(views.body(R.string.dk_created_body))
        keySummary(key.file)
        column.addView(views.button(R.string.dk_export, KeyViews.Style.PRIMARY) { chooseExport(key) })
        column.addView(views.button(R.string.dk_done, KeyViews.Style.SECONDARY) { render() })
    }

    /** The ID in grouped monospace, copyable by a button or a long press, then label and passphrase state. */
    private fun keySummary(file: KeyFile) {
        column.addView(views.fieldLabel(R.string.dk_id))
        column.addView(TextView(this).apply {
            text = file.id
            typeface = Typeface.MONOSPACE
            setTextSize(TypedValue.COMPLEX_UNIT_SP, 20f)
            setTextColor(KeyPalette.MILK)
            setPadding(0, views.dp(4), 0, views.dp(4))
            isLongClickable = true
            setOnLongClickListener {
                copyId(file.id)
                true
            }
        })
        column.addView(views.button(R.string.dk_id_copy, KeyViews.Style.SECONDARY) { copyId(file.id) })
        column.addView(views.fieldLabel(R.string.dk_label))
        column.addView(views.body(file.label))
        column.addView(views.fieldLabel(R.string.dk_passphrase))
        column.addView(views.body(if (file.isProtected) R.string.dk_has_passphrase else R.string.dk_no_passphrase))
    }

    // ---- create -----------------------------------------------------------------------------------

    private fun createKey(name: String, pass: String, again: String, button: TextView) {
        if (pass != again) {
            showError(R.string.dk_passphrase_mismatch)
            return
        }
        setBusy(button, busy = true, text = R.string.dk_creating)
        val label = name.trim().ifEmpty { Build.MODEL }
        worker.execute {
            val result = runCatching {
                val created = KeyFileCodec.create(label, pass.ifEmpty { null })
                created.key.close()
                val text = KeyFileCodec.serialize(created.file)
                store.savePortable(text)
                StoredKey.Present(text, created.file)
            }
            runOnUiThread {
                if (isDestroyed) return@runOnUiThread
                result.onSuccess { key ->
                    stored = key
                    showCreated(key)
                }.onFailure {
                    setBusy(button, busy = false, text = R.string.dk_create)
                    showError(DeviceKeyLogic.messageFor(it))
                }
            }
        }
    }

    // ---- export -----------------------------------------------------------------------------------

    private fun chooseExport(key: StoredKey.Present) {
        val options = if (key.file.isProtected) {
            listOf(
                R.string.dk_export_keep to Export.KEEP,
                R.string.dk_export_new to Export.NEW,
                R.string.dk_export_none to Export.NONE,
            )
        } else {
            listOf(
                R.string.dk_export_plain to Export.KEEP,
                R.string.dk_export_add to Export.NEW,
            )
        }
        AlertDialog.Builder(this)
            .setTitle(R.string.dk_export_choice_title)
            .setItems(options.map { getString(it.first) }.toTypedArray()) { _, which ->
                when (val choice = options[which].second) {
                    Export.KEEP -> saveAs(key.text, key.file.id)
                    Export.NEW, Export.NONE -> reseal(key, askNewPassphrase = choice == Export.NEW) {
                        saveAs(it, key.file.id)
                    }
                }
            }
            .setNegativeButton(R.string.dk_cancel, null)
            .show()
    }

    /** Opens the save picker for [text]. The file is written only after the user picks a place. */
    private fun saveAs(text: String, id: String) {
        pendingExport = text
        startActivityForResult(
            Intent(Intent.ACTION_CREATE_DOCUMENT)
                .addCategory(Intent.CATEGORY_OPENABLE)
                .setType(MIME_KEY)
                .putExtra(Intent.EXTRA_TITLE, DeviceKeyLogic.exportFileName(id)),
            REQUEST_EXPORT,
        )
    }

    private fun writeExport(uri: Uri, text: String) {
        worker.execute {
            val result = runCatching {
                val bytes = text.toByteArray(Charsets.UTF_8)
                try {
                    val out = contentResolver.openOutputStream(uri, "wt") ?: throw IOException("no output stream")
                    out.use { it.write(bytes) }
                } finally {
                    bytes.fill(0)
                }
            }
            runOnUiThread {
                if (isDestroyed) return@runOnUiThread
                val message = if (result.isSuccess) R.string.dk_exported else R.string.dk_export_failed
                Toast.makeText(this, message, Toast.LENGTH_LONG).show()
            }
        }
    }

    // ---- import -----------------------------------------------------------------------------------

    private fun pickImport() {
        startActivityForResult(
            Intent(Intent.ACTION_OPEN_DOCUMENT)
                .addCategory(Intent.CATEGORY_OPENABLE)
                .setType("*/*")
                .putExtra(Intent.EXTRA_MIME_TYPES, arrayOf(MIME_KEY, "text/plain", "application/octet-stream")),
            REQUEST_IMPORT,
        )
    }

    private fun readImport(uri: Uri) {
        worker.execute {
            val result = runCatching { readKeyFile(uri) }
            runOnUiThread {
                if (isDestroyed) return@runOnUiThread
                result.onSuccess { (file, text) -> confirmReplace(file, text) }
                    .onFailure { showError(DeviceKeyLogic.messageFor(it)) }
            }
        }
    }

    /** Reads and parses a picked file. Nothing is unlocked here, so this is cheap apart from the read. */
    private fun readKeyFile(uri: Uri): Pair<KeyFile, String> {
        val bytes = contentResolver.openInputStream(uri)?.use {
            DeviceKeyLogic.readBounded(it, MAX_FILE_BYTES)
        } ?: throw IOException("no input stream")
        try {
            val text = try {
                Charsets.UTF_8.newDecoder()
                    .onMalformedInput(CodingErrorAction.REPORT)
                    .onUnmappableCharacter(CodingErrorAction.REPORT)
                    .decode(ByteBuffer.wrap(bytes))
                    .toString()
            } catch (e: CharacterCodingException) {
                throw DeviceKeyException(DeviceKeyException.Reason.MALFORMED, "key file is not UTF-8")
            }
            return KeyFileCodec.parse(text) to text
        } finally {
            bytes.fill(0)
        }
    }

    /** Importing over a key that exists needs a yes first. A missing key goes straight on. */
    private fun confirmReplace(file: KeyFile, text: String) {
        if (stored is StoredKey.Missing) {
            continueImport(file, text)
            return
        }
        AlertDialog.Builder(this)
            .setTitle(R.string.dk_replace_title)
            .setMessage(R.string.dk_replace_message)
            .setPositiveButton(R.string.dk_replace_action) { _, _ -> continueImport(file, text) }
            .setNegativeButton(R.string.dk_cancel, null)
            .show()
    }

    private fun continueImport(file: KeyFile, text: String) {
        unlockCurrent(file, R.string.dk_import_title, R.string.dk_import_message, R.string.dk_import_action) { key ->
            // Unlocking proves the passphrase and the key match before anything is replaced.
            key.close()
            writeStored(text, R.string.dk_imported)
        }
    }

    // ---- passphrase changes -----------------------------------------------------------------------

    /**
     * Unlocks the stored key (asking for its passphrase when it has one), then re-seals it under a
     * new passphrase, or none, and hands the portable text to [then]. The unlocked seed is closed on
     * every path.
     */
    private fun reseal(key: StoredKey.Present, askNewPassphrase: Boolean, then: (String) -> Unit) {
        unlockCurrent(key.file, R.string.dk_unlock_title, R.string.dk_unlock_message, R.string.dk_unlock_action) { unlocked ->
            fun commit(newPassphrase: String?, dialog: PassphraseDialog?) {
                worker.execute {
                    val result = runCatching {
                        try {
                            KeyFileCodec.serialize(KeyFileCodec.rewrap(key.file, unlocked, newPassphrase))
                        } finally {
                            unlocked.close()
                        }
                    }
                    runOnUiThread {
                        if (isDestroyed) return@runOnUiThread
                        result.onSuccess { text ->
                            dialog?.finish()
                            then(text)
                        }.onFailure {
                            val message = DeviceKeyLogic.messageFor(it)
                            if (dialog != null) dialog.fail(message) else showError(message)
                        }
                    }
                }
            }
            if (askNewPassphrase) {
                PassphraseDialog(
                    activity = this,
                    title = R.string.dk_new_title,
                    message = R.string.dk_new_message,
                    confirm = true,
                    action = R.string.dk_continue,
                    onSubmit = { pass, dialog -> commit(pass.ifEmpty { null }, dialog) },
                    onCancel = { unlocked.close() },
                ).show()
            } else {
                commit(null, null)
            }
        }
    }

    /**
     * Unlocks [file] and passes the open key to [onUnlocked]; the caller closes it. A protected file
     * asks for its passphrase in a [PassphraseDialog] (busy while Argon2id runs; a wrong one is
     * shown inline and can be retried).
     */
    private fun unlockCurrent(
        file: KeyFile,
        title: Int,
        message: Int,
        action: Int,
        onUnlocked: (DeviceKey) -> Unit,
    ) {
        if (!file.isProtected) {
            worker.execute {
                val result = runCatching { KeyFileCodec.unlock(file, null) }
                runOnUiThread {
                    if (isDestroyed) {
                        result.getOrNull()?.close()
                        return@runOnUiThread
                    }
                    result.onSuccess(onUnlocked).onFailure { showError(DeviceKeyLogic.messageFor(it)) }
                }
            }
            return
        }
        PassphraseDialog(
            activity = this,
            title = title,
            message = message,
            confirm = false,
            action = action,
            onSubmit = { pass, dialog ->
                worker.execute {
                    val result = runCatching { KeyFileCodec.unlock(file, pass) }
                    runOnUiThread {
                        if (isDestroyed) {
                            result.getOrNull()?.close()
                            return@runOnUiThread
                        }
                        result.onSuccess { key ->
                            dialog.finish()
                            onUnlocked(key)
                        }.onFailure { dialog.fail(DeviceKeyLogic.messageFor(it)) }
                    }
                }
            },
            onCancel = {},
        ).show()
    }

    // ---- store changes ----------------------------------------------------------------------------

    /** Saves [text] as this phone's key, then shows the result with [done] as its notice. */
    private fun writeStored(text: String, done: Int) {
        worker.execute {
            val result = runCatching { store.savePortable(text) }
            runOnUiThread {
                if (isDestroyed) return@runOnUiThread
                result.onSuccess {
                    notice = done
                    reload()
                }.onFailure { showError(DeviceKeyLogic.messageFor(it)) }
            }
        }
    }

    private fun confirmRemove() {
        AlertDialog.Builder(this)
            .setTitle(R.string.dk_remove_title)
            .setMessage(R.string.dk_remove_message)
            .setPositiveButton(R.string.dk_remove_action) { _, _ -> removeKey() }
            .setNegativeButton(R.string.dk_cancel, null)
            .show()
    }

    /**
     * Deletes the key file. The store has no remove call, so this deletes the same file names it
     * uses (the file and AtomicFile's backup) directly from app-private storage.
     */
    private fun removeKey() {
        worker.execute {
            val result = runCatching {
                val stuck = listOf(STORE_FILE, "$STORE_FILE.bak")
                    .map { File(filesDir, it) }
                    .filter { it.exists() && !it.delete() }
                if (stuck.isNotEmpty()) throw IOException("could not delete the key file")
            }
            runOnUiThread {
                if (isDestroyed) return@runOnUiThread
                result.onSuccess {
                    notice = R.string.dk_removed
                    reload()
                }.onFailure { showError(R.string.dk_err_io) }
            }
        }
    }

    // ---- shared helpers ---------------------------------------------------------------------------

    /** Re-reads the store on the worker, then runs [after] on the UI thread (default: redraw). */
    private fun reload(after: () -> Unit = { render() }) {
        worker.execute {
            val current = DeviceKeyLogic.read(store)
            runOnUiThread {
                if (isDestroyed) return@runOnUiThread
                stored = current
                after()
            }
        }
    }

    private fun showError(message: Int) {
        val line = status
        if (line != null) views.tell(line, message, error = true)
        else Toast.makeText(this, message, Toast.LENGTH_LONG).show()
    }

    private fun setBusy(button: TextView, busy: Boolean, text: Int) {
        button.isEnabled = !busy
        button.setText(text)
    }

    private fun copyId(id: String) {
        val clipboard = getSystemService(Context.CLIPBOARD_SERVICE) as ClipboardManager
        clipboard.setPrimaryClip(ClipData.newPlainText(getString(R.string.dk_id), id))
        Toast.makeText(this, R.string.dk_id_copied, Toast.LENGTH_SHORT).show()
    }

    override fun onActivityResult(requestCode: Int, resultCode: Int, data: Intent?) {
        super.onActivityResult(requestCode, resultCode, data)
        val uri = data?.data
        when (requestCode) {
            REQUEST_EXPORT -> {
                val text = pendingExport
                pendingExport = null
                if (resultCode == RESULT_OK && uri != null && text != null) writeExport(uri, text)
            }
            REQUEST_IMPORT -> if (resultCode == RESULT_OK && uri != null) readImport(uri)
        }
    }

    companion object {
        /** Start-screen entry points: create goes straight to the form, import to the picker. */
        const val EXTRA_START = "deviceKeyStart"
        const val START_CREATE = "create"
        const val START_IMPORT = "import"

        private const val REQUEST_EXPORT = 1
        private const val REQUEST_IMPORT = 2
        private const val MIME_KEY = "application/json"
        /** The store's file name (keys/DeviceKeyStore). Kept in step with it: Remove deletes this file. */
        private const val STORE_FILE = "device-key.bpkey"
        private const val MAX_FILE_BYTES = 64 * 1024

        fun intent(context: Context, start: String? = null): Intent =
            Intent(context, DeviceKeyActivity::class.java).apply {
                if (start != null) putExtra(EXTRA_START, start)
            }
    }
}
