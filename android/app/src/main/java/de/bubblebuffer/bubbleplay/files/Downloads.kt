package de.bubblebuffer.bubbleplay.files

import android.Manifest
import android.app.NotificationChannel
import android.app.NotificationManager
import android.app.PendingIntent
import android.content.ContentValues
import android.content.Context
import android.content.Intent
import android.content.pm.PackageManager
import android.net.Uri
import android.os.Build
import android.os.Environment
import android.provider.MediaStore
import de.bubblebuffer.bubbleplay.R
import java.io.IOException
import java.io.OutputStream

/**
 * Downloads go to Download/BubblePlay through MediaStore (API 29+, no storage
 * permission). The entry stays IS_PENDING (invisible to other apps) until the
 * size and SHA-256 check passed; a failed or cancelled transfer deletes it.
 */
class DownloadStore(private val context: Context) {
    class Target(val uri: Uri, val stream: OutputStream, val mime: String)

    class Published(val uri: Uri, val name: String, val mime: String)

    fun create(name: String, mime: String): Target {
        val values = ContentValues().apply {
            put(MediaStore.MediaColumns.DISPLAY_NAME, name)
            put(MediaStore.MediaColumns.MIME_TYPE, mime)
            put(MediaStore.MediaColumns.RELATIVE_PATH, "${Environment.DIRECTORY_DOWNLOADS}/$FOLDER")
            put(MediaStore.MediaColumns.IS_PENDING, 1)
        }
        val resolver = context.contentResolver
        val uri = resolver.insert(MediaStore.Downloads.EXTERNAL_CONTENT_URI, values) ?: throw IOException("insert refused")
        val stream = try { resolver.openOutputStream(uri, "w") } catch (e: Exception) { null }
        if (stream == null) {
            resolver.delete(uri, null, null)
            throw IOException("no output stream")
        }
        return Target(uri, stream.buffered(256 * 1024), mime)
    }

    fun publish(target: Target): Published {
        target.stream.close()
        val resolver = context.contentResolver
        resolver.update(target.uri, ContentValues().apply { put(MediaStore.MediaColumns.IS_PENDING, 0) }, null, null)
        // MediaStore may have renamed it (" (1)" on a collision, or a fixed extension).
        val name = resolver.query(target.uri, arrayOf(MediaStore.MediaColumns.DISPLAY_NAME), null, null, null)?.use { c ->
            if (c.moveToFirst()) c.getString(0) else null
        } ?: FileNames.FALLBACK_DOWNLOAD
        return Published(target.uri, name, target.mime)
    }

    fun discard(target: Target) {
        try { target.stream.close() } catch (_: IOException) {}
        try { context.contentResolver.delete(target.uri, null, null) } catch (_: Exception) {}
    }

    companion object {
        const val FOLDER = "BubblePlay"
    }
}

/** "Saved to Downloads" notification with Open and Share. */
object DownloadNotifier {
    private const val CHANNEL = "downloads"

    fun canNotify(context: Context): Boolean =
        Build.VERSION.SDK_INT < Build.VERSION_CODES.TIRAMISU ||
            context.checkSelfPermission(Manifest.permission.POST_NOTIFICATIONS) == PackageManager.PERMISSION_GRANTED

    fun openIntent(uri: Uri, mime: String): Intent = Intent(Intent.ACTION_VIEW)
        .setDataAndType(uri, mime)
        .addFlags(Intent.FLAG_GRANT_READ_URI_PERMISSION or Intent.FLAG_ACTIVITY_NEW_TASK)

    fun shareIntent(context: Context, uri: Uri, mime: String): Intent = Intent.createChooser(
        Intent(Intent.ACTION_SEND).setType(mime).putExtra(Intent.EXTRA_STREAM, uri).addFlags(Intent.FLAG_GRANT_READ_URI_PERMISSION),
        context.getString(R.string.file_share_title),
    ).addFlags(Intent.FLAG_ACTIVITY_NEW_TASK or Intent.FLAG_GRANT_READ_URI_PERMISSION)

    fun notify(context: Context, name: String, uri: Uri, mime: String) {
        if (!canNotify(context)) return
        val manager = context.getSystemService(NotificationManager::class.java) ?: return
        if (manager.getNotificationChannel(CHANNEL) == null) {
            manager.createNotificationChannel(
                NotificationChannel(CHANNEL, context.getString(R.string.file_channel), NotificationManager.IMPORTANCE_DEFAULT),
            )
        }
        val id = uri.hashCode()
        val flags = PendingIntent.FLAG_IMMUTABLE or PendingIntent.FLAG_UPDATE_CURRENT
        val open = PendingIntent.getActivity(context, id * 2, openIntent(uri, mime), flags)
        val share = PendingIntent.getActivity(context, id * 2 + 1, shareIntent(context, uri, mime), flags)
        val notification = android.app.Notification.Builder(context, CHANNEL)
            .setSmallIcon(R.drawable.ic_notify_download)
            .setContentTitle(name)
            .setContentText(context.getString(R.string.file_saved_downloads))
            .setContentIntent(open)
            .setAutoCancel(true)
            .addAction(android.app.Notification.Action.Builder(null, context.getString(R.string.file_open), open).build())
            .addAction(android.app.Notification.Action.Builder(null, context.getString(R.string.file_share), share).build())
            .build()
        manager.notify(id, notification)
    }
}
