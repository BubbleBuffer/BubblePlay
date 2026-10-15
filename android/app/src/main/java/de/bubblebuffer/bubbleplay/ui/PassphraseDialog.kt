package de.bubblebuffer.bubbleplay.ui

import android.app.Activity
import android.app.AlertDialog
import android.view.View
import android.widget.LinearLayout
import de.bubblebuffer.bubbleplay.R

/**
 * The one passphrase prompt, for the unlock, import and new-passphrase steps.
 *
 * [onSubmit] receives the raw entry and must do its Argon2id work off the main thread, then call
 * [finish] or [fail] on the main thread. While work runs the dialog is busy: fields and buttons are
 * disabled and it cannot be dismissed. A dismissal that did not come from [finish] calls [onCancel],
 * so the caller can wipe whatever it is holding.
 */
internal class PassphraseDialog(
    activity: Activity,
    title: Int,
    message: Int,
    private val confirm: Boolean,
    private val action: Int,
    private val onSubmit: (passphrase: String, dialog: PassphraseDialog) -> Unit,
    private val onCancel: () -> Unit,
) {
    private val views = KeyViews(activity)
    private val entry = views.passwordField(R.string.dk_passphrase_hint)
    private val again = views.passwordField(R.string.dk_passphrase_repeat)
    private val problem = views.status()
    private var completed = false
    private val dialog: AlertDialog

    init {
        val content = LinearLayout(activity).apply {
            orientation = LinearLayout.VERTICAL
            setPadding(views.dp(20), views.dp(8), views.dp(20), 0)
            addView(views.body(message))
            addView(entry)
            if (confirm) addView(again)
            addView(problem)
        }
        dialog = AlertDialog.Builder(activity)
            .setTitle(title)
            .setView(content)
            .setPositiveButton(action, null)
            .setNegativeButton(R.string.dk_cancel, null)
            .create()
        // The positive button is wired after show, so a failed attempt keeps the dialog open.
        dialog.setOnShowListener { dialog.getButton(AlertDialog.BUTTON_POSITIVE).setOnClickListener { submit() } }
        dialog.setOnDismissListener { if (!completed) onCancel() }
    }

    fun show() = dialog.show()

    /** The work succeeded: closes the dialog. */
    fun finish() {
        completed = true
        dialog.dismiss()
    }

    /** The work failed: shows [message] in the dialog and re-enables it for another try. */
    fun fail(message: Int) {
        busy(false)
        views.tell(problem, message, error = true)
    }

    private fun submit() {
        if (confirm && entry.text.toString() != again.text.toString()) {
            views.tell(problem, R.string.dk_passphrase_mismatch, error = true)
            return
        }
        problem.visibility = View.GONE
        busy(true)
        onSubmit(entry.text.toString(), this)
    }

    private fun busy(on: Boolean) {
        entry.isEnabled = !on
        again.isEnabled = !on
        dialog.setCancelable(!on)
        dialog.getButton(AlertDialog.BUTTON_NEGATIVE).isEnabled = !on
        dialog.getButton(AlertDialog.BUTTON_POSITIVE).apply {
            isEnabled = !on
            setText(if (on) R.string.dk_working else action)
        }
    }
}
