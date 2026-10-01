package de.bubblebuffer.bubbleplay.ui

import android.app.Activity
import android.graphics.Typeface
import android.graphics.drawable.ColorDrawable
import android.graphics.drawable.Drawable
import android.graphics.drawable.GradientDrawable
import android.graphics.drawable.StateListDrawable
import android.net.Uri
import android.os.Handler
import android.os.Looper
import android.text.TextUtils
import android.text.format.Formatter
import android.util.TypedValue
import android.view.Gravity
import android.view.View
import android.view.ViewGroup
import android.widget.FrameLayout
import android.widget.ImageButton
import android.widget.ImageView
import android.widget.LinearLayout
import android.widget.TextView
import de.bubblebuffer.bubbleplay.R
import de.bubblebuffer.bubbleplay.files.FileTransfers
import de.bubblebuffer.bubbleplay.protocol.FileWire
import kotlin.math.roundToInt

/**
 * File transfer rows docked above the quick-action bar (never over the
 * video; the stream is laid out above the whole dock): a notice with Open /
 * Share, the PC's offer chip ("2 files from PC · Download"), and one slim
 * progress row per direction with percent and cancel.
 */
class TransferDock(private val activity: Activity, dock: LinearLayout, private val host: Host) {
    interface Host {
        fun downloadOffer()
        fun dismissOffer()
        fun cancelTransfer(direction: FileTransfers.Direction)
        fun openDownload(uri: Uri, mime: String)
        fun shareDownload(uri: Uri, mime: String)
    }

    private val density = activity.resources.displayMetrics.density
    private val timers = Handler(Looper.getMainLooper())

    private val notice: LinearLayout
    private val noticeText: TextView
    private val noticeOpen: TextView
    private val noticeShare: TextView
    private val offer: LinearLayout
    private val offerText: TextView
    private val upRow: ProgressRow
    private val downRow: ProgressRow
    private val hideNotice = Runnable { notice.visibility = View.GONE }

    init {
        // Notice ("Saved photo.jpg to Downloads · Open · Share").
        notice = row()
        noticeText = label(MILK)
        notice.addView(noticeText, LinearLayout.LayoutParams(0, ViewGroup.LayoutParams.WRAP_CONTENT, 1f))
        noticeOpen = textButton(R.string.file_open, MILK)
        noticeShare = textButton(R.string.file_share, MILK)
        notice.addView(noticeOpen)
        notice.addView(noticeShare)
        notice.addView(iconButton(R.drawable.ic_bar_close, R.string.file_dismiss) { timers.removeCallbacks(hideNotice); notice.visibility = View.GONE })

        // Offer chip.
        offer = row()
        offer.addView(icon(R.drawable.ic_notify_download, CREAM))
        offerText = label(MILK)
        offer.addView(offerText, LinearLayout.LayoutParams(0, ViewGroup.LayoutParams.WRAP_CONTENT, 1f))
        offer.addView(textButton(R.string.file_download, DUVET, primary = true).apply { setOnClickListener { host.downloadOffer() } })
        offer.addView(iconButton(R.drawable.ic_bar_close, R.string.file_offer_dismiss) { host.dismissOffer() })

        upRow = ProgressRow(FileTransfers.Direction.UP)
        downRow = ProgressRow(FileTransfers.Direction.DOWN)

        for (view in listOf(notice, offer, upRow.view, downRow.view)) {
            view.visibility = View.GONE
            dock.addView(view, LinearLayout.LayoutParams(ViewGroup.LayoutParams.MATCH_PARENT, dp(48)))
        }
    }

    // ---- state ------------------------------------------------------------------------------

    fun showOffer(offer: FileWire.Offer?) {
        if (offer == null || offer.items.isEmpty()) { this.offer.visibility = View.GONE; return }
        val count = offer.items.size
        val size = Formatter.formatShortFileSize(activity, offer.totalBytes)
        offerText.text = if (count == 1) activity.getString(R.string.file_offer_one, offer.items[0].name, size)
        else activity.resources.getQuantityString(R.plurals.file_offer_many, count, count, size)
        this.offer.contentDescription = offerText.text
        this.offer.visibility = View.VISIBLE
    }

    fun showProgress(up: FileTransfers.Row?, down: FileTransfers.Row?) {
        upRow.render(up)
        downRow.render(down)
    }

    /** A transient message; Open/Share appear when [uri] is given. Hides after a few seconds. */
    fun showNotice(text: String, uri: Uri? = null, mime: String? = null) {
        noticeText.text = text
        val actions = uri != null && mime != null
        noticeOpen.visibility = if (actions) View.VISIBLE else View.GONE
        noticeShare.visibility = if (actions) View.VISIBLE else View.GONE
        if (uri != null && mime != null) {
            noticeOpen.setOnClickListener { host.openDownload(uri, mime) }
            noticeShare.setOnClickListener { host.shareDownload(uri, mime) }
        }
        notice.visibility = View.VISIBLE
        notice.announceForAccessibility(text)
        timers.removeCallbacks(hideNotice)
        timers.postDelayed(hideNotice, if (actions) NOTICE_ACTION_MS else NOTICE_MS)
    }

    /** No host file transfer (or the session ended): hide everything. */
    fun clear() {
        offer.visibility = View.GONE
        upRow.render(null)
        downRow.render(null)
    }

    fun onDestroy() = timers.removeCallbacksAndMessages(null)

    // ---- progress row -------------------------------------------------------------------------

    private inner class ProgressRow(private val direction: FileTransfers.Direction) {
        val view = FrameLayout(activity).apply { background = rowBackground() }
        private val line = LinearLayout(activity).apply {
            orientation = LinearLayout.HORIZONTAL
            gravity = Gravity.CENTER_VERTICAL
            setPadding(dp(12), 0, 0, 0)
        }
        private val name = label(MILK).apply { ellipsize = TextUtils.TruncateAt.MIDDLE }
        private val queued = mono(CREAM)
        private val percent = mono(MILK)
        private val bar = View(activity).apply { setBackgroundColor(MILK) }

        init {
            line.addView(icon(if (direction == FileTransfers.Direction.UP) R.drawable.ic_file_upload else R.drawable.ic_notify_download, CREAM))
            line.addView(name, LinearLayout.LayoutParams(0, ViewGroup.LayoutParams.WRAP_CONTENT, 1f))
            line.addView(queued)
            line.addView(percent)
            line.addView(iconButton(R.drawable.ic_bar_close, if (direction == FileTransfers.Direction.UP) R.string.file_cancel_upload else R.string.file_cancel_download) {
                host.cancelTransfer(direction)
            })
            view.addView(line, FrameLayout.LayoutParams(ViewGroup.LayoutParams.MATCH_PARENT, ViewGroup.LayoutParams.MATCH_PARENT))
            // Progress is a 2 dp seam along the bottom edge.
            view.addView(bar, FrameLayout.LayoutParams(0, dp(2), Gravity.BOTTOM or Gravity.START))
        }

        fun render(row: FileTransfers.Row?) {
            if (row == null) { view.visibility = View.GONE; return }
            view.visibility = View.VISIBLE
            name.text = row.name
            queued.text = if (row.queued > 0) activity.getString(R.string.file_queued, row.queued) else ""
            queued.visibility = if (row.queued > 0) View.VISIBLE else View.GONE
            percent.text = when (row.phase) {
                FileTransfers.Phase.WAITING -> activity.getString(R.string.file_waiting)
                FileTransfers.Phase.VERIFYING -> activity.getString(R.string.file_verifying)
                FileTransfers.Phase.ACTIVE -> activity.getString(R.string.file_percent, row.percent)
            }
            val width = view.width
            val lp = bar.layoutParams
            val target = if (width > 0) (width * row.percent / 100f).roundToInt() else 0
            if (lp.width != target) { lp.width = target; bar.layoutParams = lp }
            val verb = activity.getString(if (direction == FileTransfers.Direction.UP) R.string.file_sending else R.string.file_receiving)
            view.contentDescription = "$verb ${row.name}, ${percent.text}"
        }
    }

    // ---- views ----------------------------------------------------------------------------------

    private fun row() = LinearLayout(activity).apply {
        orientation = LinearLayout.HORIZONTAL
        gravity = Gravity.CENTER_VERTICAL
        setPadding(dp(12), 0, 0, 0)
        background = rowBackground()
    }

    /** Surface with a 1 dp thread seam on top: a change of context above the bar. */
    private fun rowBackground(): Drawable = android.graphics.drawable.LayerDrawable(arrayOf(
        ColorDrawable(THREAD),
        ColorDrawable(SURFACE),
    )).apply { setLayerInset(1, 0, dp(1), 0, 0) }

    private fun label(color: Int) = TextView(activity).apply {
        setTextColor(color)
        setTextSize(TypedValue.COMPLEX_UNIT_SP, 14f)
        isSingleLine = true
        ellipsize = TextUtils.TruncateAt.END
        setPadding(dp(8), 0, dp(8), 0)
    }

    private fun mono(color: Int) = TextView(activity).apply {
        typeface = Typeface.MONOSPACE
        setTextColor(color)
        setTextSize(TypedValue.COMPLEX_UNIT_SP, 12f)
        setPadding(dp(4), 0, dp(4), 0)
    }

    private fun icon(res: Int, tint: Int) = ImageView(activity).apply {
        setImageResource(res)
        imageTintList = android.content.res.ColorStateList.valueOf(tint)
        importantForAccessibility = View.IMPORTANT_FOR_ACCESSIBILITY_NO
    }

    private fun iconButton(res: Int, description: Int, onClick: () -> Unit) = ImageButton(activity).apply {
        setImageResource(res)
        imageTintList = android.content.res.ColorStateList.valueOf(MILK)
        scaleType = ImageView.ScaleType.CENTER
        background = pressable()
        contentDescription = activity.getString(description)
        tooltipText = contentDescription
        isFocusable = false
        setOnClickListener { onClick() }
        layoutParams = LinearLayout.LayoutParams(dp(48), dp(48))
    }

    private fun textButton(label: Int, color: Int, primary: Boolean = false) = TextView(activity).apply {
        setText(label)
        setTextColor(color)
        setTextSize(TypedValue.COMPLEX_UNIT_SP, 14f)
        gravity = Gravity.CENTER
        minWidth = dp(48)
        setPadding(dp(12), 0, dp(12), 0)
        isClickable = true
        isFocusable = false
        background = if (primary) StateListDrawable().apply {
            addState(intArrayOf(android.R.attr.state_pressed), shape(CREAM))
            addState(intArrayOf(), shape(MILK))
        } else pressable()
        layoutParams = LinearLayout.LayoutParams(ViewGroup.LayoutParams.WRAP_CONTENT, dp(if (primary) 36 else 48)).apply {
            if (primary) setMargins(dp(4), 0, dp(4), 0)
        }
    }

    private fun pressable(): Drawable = StateListDrawable().apply {
        addState(intArrayOf(android.R.attr.state_pressed), shape(SURFACE_MUTED))
        addState(intArrayOf(), ColorDrawable(android.graphics.Color.TRANSPARENT))
    }

    private fun shape(fill: Int) = GradientDrawable().apply {
        cornerRadii = floatArrayOf(7f, 7f, 11f, 11f, 6f, 6f, 9f, 9f).map { it * density }.toFloatArray()
        setColor(fill)
    }

    private fun dp(value: Int) = (value * density).roundToInt()

    private companion object {
        // BubblePlay dark palette (bubblebuffer-design), as in QuickBar.
        const val DUVET = 0xFF2F3138.toInt()
        const val SURFACE = 0xFF30333A.toInt()
        const val SURFACE_MUTED = 0xFF3B3E45.toInt()
        const val THREAD = 0xFF575B64.toInt()
        const val MILK = 0xFFF3EFE7.toInt()
        const val CREAM = 0xFFC6BAAC.toInt()
        const val NOTICE_MS = 4_000L
        const val NOTICE_ACTION_MS = 8_000L
    }
}
