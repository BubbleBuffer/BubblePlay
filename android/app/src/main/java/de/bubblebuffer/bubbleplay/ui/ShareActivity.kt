package de.bubblebuffer.bubbleplay.ui

import android.app.Activity
import android.content.ClipData
import android.content.Intent
import android.net.Uri
import android.os.Build
import android.os.Bundle
import android.widget.Toast
import de.bubblebuffer.bubbleplay.R
import de.bubblebuffer.bubbleplay.files.ShareParse

/**
 * Share-sheet target ("BubblePlay" for any MIME type). No UI: it hands the shared URIs to the PC
 * list ([MainActivity]), which keeps them until the user picks a PC; the stream screen then uploads
 * them (purpose "save") once connected. The device key gate and pairing happen there, so a share
 * never skips them.
 *
 * Read access to the URIs was granted to this activity; every hop forwards it
 * with FLAG_GRANT_READ_URI_PERMISSION + ClipData ([PendingShares]).
 */
class ShareActivity : Activity() {
    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        val result = parse(intent)
        if (result.items.isEmpty()) {
            Toast.makeText(this, R.string.file_nothing, Toast.LENGTH_LONG).show()
            finish(); return
        }
        if (result.dropped > 0) Toast.makeText(this, resources.getQuantityString(R.plurals.file_too_many, result.dropped, result.dropped), Toast.LENGTH_LONG).show()
        // Into BubblePlay's own task: an open PC list gets onNewIntent.
        val forward = Intent(this, MainActivity::class.java).addFlags(Intent.FLAG_ACTIVITY_NEW_TASK or Intent.FLAG_ACTIVITY_CLEAR_TOP or Intent.FLAG_ACTIVITY_SINGLE_TOP)
        startActivity(PendingShares.attach(forward, result.items))
        finish()
    }

    @Suppress("DEPRECATION")
    private fun parse(intent: Intent): ShareParse.Result<Uri> {
        val stream: Uri? = if (Build.VERSION.SDK_INT >= 33) intent.getParcelableExtra(Intent.EXTRA_STREAM, Uri::class.java)
        else intent.getParcelableExtra(Intent.EXTRA_STREAM) as? Uri
        val streams: List<Uri?>? = if (Build.VERSION.SDK_INT >= 33) intent.getParcelableArrayListExtra(Intent.EXTRA_STREAM, Uri::class.java)
        else intent.getParcelableArrayListExtra(Intent.EXTRA_STREAM)
        val clip = intent.clipData?.let { c -> (0 until c.itemCount).map { c.getItemAt(it).uri } }.orEmpty()
        return ShareParse.collect(intent.action, if (intent.action == Intent.ACTION_SEND) stream else null,
            if (intent.action == Intent.ACTION_SEND_MULTIPLE) streams else null, clip)
    }
}

/** Shared URIs riding along an Intent (ClipData + read grant), from the share sheet to the stream screen. */
object PendingShares {
    private const val EXTRA = "de.bubblebuffer.bubbleplay.shares"

    fun attach(intent: Intent, uris: List<Uri>): Intent {
        if (uris.isEmpty()) return intent
        val clip = ClipData.newRawUri("shared", uris[0])
        for (i in 1 until uris.size) clip.addItem(ClipData.Item(uris[i]))
        intent.clipData = clip
        intent.addFlags(Intent.FLAG_GRANT_READ_URI_PERMISSION)
        intent.putExtra(EXTRA, true)
        return intent
    }

    fun read(intent: Intent?): List<Uri> {
        if (intent == null || !intent.getBooleanExtra(EXTRA, false)) return emptyList()
        val clip = intent.clipData ?: return emptyList()
        return (0 until clip.itemCount).mapNotNull { clip.getItemAt(it).uri }
    }

    /** Clears the marker so a re-delivered intent (recreation) does not upload twice. */
    fun consume(intent: Intent?) {
        intent?.removeExtra(EXTRA)
    }
}
