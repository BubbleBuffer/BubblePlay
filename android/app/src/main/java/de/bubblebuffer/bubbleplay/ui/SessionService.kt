package de.bubblebuffer.bubbleplay.ui

import android.app.Notification
import android.app.NotificationChannel
import android.app.NotificationManager
import android.app.PendingIntent
import android.app.Service
import android.content.Context
import android.content.Intent
import android.content.pm.ServiceInfo
import android.os.Build
import android.os.IBinder
import android.util.Log
import de.bubblebuffer.bubbleplay.R

/**
 * Background survival: a foreground service (type dataSync) that keeps the
 * process at foreground priority while a stream session is open, so its
 * sockets and file transfers survive when the app is in the background. The
 * session itself stays in [StreamActivity]; this service only holds the
 * notification (tap: back to the stream; Disconnect: end the session).
 *
 * It is started with the session, while the activity is in the foreground
 * (Android refuses foreground services started from the background), and
 * stopped with it. When Android refuses to start it, or ends it (the dataSync
 * time limit on Android 15+), the activity falls back to the 30 s background
 * stop ([KeepAlive.Owner.onKeepAliveEnded]).
 */
class SessionService : Service() {
    override fun onBind(intent: Intent?): IBinder? = null

    override fun onStartCommand(intent: Intent?, flags: Int, startId: Int): Int {
        if (intent?.action == ACTION_DISCONNECT) {
            KeepAlive.disconnectRequested()
            stopForegroundCompat()
            stopSelf()
            return START_NOT_STICKY
        }
        val host = intent?.getStringExtra(EXTRA_HOST).orEmpty()
        try {
            val notification = notification(host)
            startForeground(NOTIFICATION_ID, notification, ServiceInfo.FOREGROUND_SERVICE_TYPE_DATA_SYNC)
            KeepAlive.started()
        } catch (e: RuntimeException) {
            // ForegroundServiceStartNotAllowedException (API 31+) and similar refusals.
            Log.w(TAG, "foreground service refused", e)
            KeepAlive.ended()
            stopSelf()
        }
        return START_NOT_STICKY
    }

    /** Android 15+: the dataSync time budget is used up; the service must stop now. */
    override fun onTimeout(startId: Int, fgsType: Int) {
        Log.i(TAG, "foreground service time limit reached")
        KeepAlive.ended()
        stopForegroundCompat()
        stopSelf()
    }

    private fun stopForegroundCompat() {
        stopForeground(STOP_FOREGROUND_REMOVE)
    }

    private fun notification(host: String): Notification {
        val manager = getSystemService(NotificationManager::class.java)
        if (manager != null && manager.getNotificationChannel(CHANNEL) == null) {
            manager.createNotificationChannel(
                NotificationChannel(CHANNEL, getString(R.string.session_channel), NotificationManager.IMPORTANCE_LOW).apply {
                    setShowBadge(false)
                },
            )
        }
        val flags = PendingIntent.FLAG_IMMUTABLE or PendingIntent.FLAG_UPDATE_CURRENT
        // Like the launcher icon: brings the existing task (and its stream screen) back as it was.
        val launch = packageManager.getLaunchIntentForPackage(packageName)
            ?.addFlags(Intent.FLAG_ACTIVITY_NEW_TASK or Intent.FLAG_ACTIVITY_RESET_TASK_IF_NEEDED)
        val open = launch?.let { PendingIntent.getActivity(this, 0, it, flags) }
        val disconnect = PendingIntent.getService(this, 1, Intent(this, SessionService::class.java).setAction(ACTION_DISCONNECT), flags)
        return Notification.Builder(this, CHANNEL)
            .setSmallIcon(R.drawable.ic_bar_monitor)
            .setContentTitle(if (host.isEmpty()) getString(R.string.session_notify_title_plain) else getString(R.string.session_notify_title, host))
            .setContentText(getString(R.string.session_notify_text))
            .setContentIntent(open)
            .setOngoing(true)
            .setShowWhen(false)
            .setCategory(Notification.CATEGORY_SERVICE)
            .addAction(Notification.Action.Builder(null, getString(R.string.session_notify_disconnect), disconnect).build())
            .apply { if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.S) setForegroundServiceBehavior(Notification.FOREGROUND_SERVICE_IMMEDIATE) }
            .build()
    }

    companion object {
        private const val TAG = "BubblePlayService"
        private const val CHANNEL = "session"
        private const val NOTIFICATION_ID = 7
        private const val ACTION_DISCONNECT = "de.bubblebuffer.bubbleplay.DISCONNECT"
        private const val EXTRA_HOST = "host"

        /** Starts the service for a session with [host]; false when Android refuses (the caller falls back). */
        fun start(context: Context, host: String): Boolean = try {
            KeepAlive.requested()
            context.startForegroundService(Intent(context, SessionService::class.java).putExtra(EXTRA_HOST, host))
            true
        } catch (e: RuntimeException) {
            Log.w(TAG, "foreground service start refused", e)
            KeepAlive.ended()
            false
        }

        fun stop(context: Context) {
            KeepAlive.stopped()
            context.stopService(Intent(context, SessionService::class.java))
        }
    }
}

/**
 * Process-wide state of [SessionService] and its link to the stream screen
 * that owns the session. Main thread only.
 */
object KeepAlive {
    interface Owner {
        /** Disconnect from the notification. */
        fun onKeepAliveDisconnect()
        /** The service was refused or ended by Android while the session runs. */
        fun onKeepAliveEnded()
    }

    var owner: Owner? = null

    /** True from a start request until the service is stopped, refused or ended. */
    var active = false
        private set

    internal fun requested() { active = true }
    internal fun started() { active = true }
    internal fun stopped() { active = false }

    internal fun ended() {
        if (!active) return
        active = false
        owner?.onKeepAliveEnded()
    }

    internal fun disconnectRequested() {
        active = false
        owner?.onKeepAliveDisconnect()
    }
}
