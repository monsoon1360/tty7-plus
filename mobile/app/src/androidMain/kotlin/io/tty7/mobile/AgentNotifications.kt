package io.tty7.mobile

import android.Manifest
import android.app.Activity
import android.app.Notification
import android.app.NotificationChannel
import android.app.NotificationManager
import android.app.PendingIntent
import android.content.Context
import android.content.Intent
import android.content.pm.PackageManager
import android.net.Uri
import android.os.Build
import android.os.Bundle

internal object AgentNotifications {
    private const val DONE = "agent_done"
    private const val WAITING = "agent_waiting"
    private const val MONITOR = "agent_monitor"
    const val MONITOR_ID = 1

    private fun manager(context: Context): NotificationManager = context.getSystemService(NotificationManager::class.java)

    fun channels(context: Context) {
        manager(context).createNotificationChannels(listOf(
            NotificationChannel(DONE, "Agent 任务完成", NotificationManager.IMPORTANCE_DEFAULT).apply { lockscreenVisibility = Notification.VISIBILITY_PRIVATE },
            NotificationChannel(WAITING, "Agent 需要人为干预", NotificationManager.IMPORTANCE_HIGH).apply { lockscreenVisibility = Notification.VISIBILITY_PRIVATE },
            NotificationChannel(MONITOR, "电脑连接", NotificationManager.IMPORTANCE_LOW).apply { setShowBadge(false) },
        ))
    }

    fun monitor(context: Context): Notification {
        channels(context)
        val open = PendingIntent.getActivity(context, 0, Intent(context, MainActivity::class.java), PendingIntent.FLAG_IMMUTABLE or PendingIntent.FLAG_UPDATE_CURRENT)
        return Notification.Builder(context, MONITOR)
            .setSmallIcon(R.drawable.ic_agent_notification)
            .setContentTitle("tty7 正在接收 Agent 通知")
            .setContentText("任务完成或需要确认、回答时通知你")
            .setContentIntent(open).setOngoing(true).setCategory(Notification.CATEGORY_SERVICE)
            .setShowWhen(false).build()
    }

    fun post(context: Context, event: AgentNotification) {
        val channel = when (event.status) {
            "done" -> DONE
            "waiting" -> WAITING
            else -> return
        }
        channels(context)
        if (Build.VERSION.SDK_INT >= 33 && context.checkSelfPermission(Manifest.permission.POST_NOTIFICATIONS) != PackageManager.PERMISSION_GRANTED) return
        if (!manager(context).areNotificationsEnabled()) return
        // Data URI is PendingIntent identity, not a browsable deep link. Exact pane identity
        // also serves as the notification tag, avoiding hash collisions between panes.
        val identity = Uri.Builder().scheme("tty7-notification").authority("pane")
            .appendPath(event.hostId).appendPath(event.workspaceId).appendPath(event.paneId.toString()).build()
        val intent = Intent(context, NotificationActivity::class.java).setData(identity)
            .putExtra("host_id", event.hostId).putExtra("workspace_id", event.workspaceId).putExtra("pane_id", event.paneId)
        val open = PendingIntent.getActivity(context, 0, intent, PendingIntent.FLAG_UPDATE_CURRENT or PendingIntent.FLAG_IMMUTABLE)
        val public = Notification.Builder(context, channel).setSmallIcon(R.drawable.ic_agent_notification)
            .setContentTitle("tty7").setContentText("Agent 状态更新").build()
        val notification = Notification.Builder(context, channel)
            .setSmallIcon(R.drawable.ic_agent_notification)
            .setContentTitle(if (event.status == "done") "${event.agentName.take(128)} · 任务完成" else "${event.agentName.take(128)} · 需要确认或回答")
            .setContentText(event.title.take(256)).setContentIntent(open).setAutoCancel(true)
            .setVisibility(Notification.VISIBILITY_PRIVATE).setPublicVersion(public)
            .setCategory(if (event.status == "waiting") Notification.CATEGORY_REMINDER else Notification.CATEGORY_STATUS)
            .build()
        try { manager(context).notify(identity.toString(), 0, notification) }
        catch (_: SecurityException) { /* Notification permission can be revoked between the check and delivery. */ }
    }
}

/** Only our immutable PendingIntents can launch this unexported entry point. */
class NotificationActivity : Activity() {
    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        val host = intent.getStringExtra("host_id")
        val workspace = intent.getStringExtra("workspace_id")
        val pane = intent.getLongExtra("pane_id", -1)
        if (host != null && workspace != null && pane >= 0) {
            AndroidRemoteOwner.get(applicationContext).controller.openNotification(host, workspace, pane)
        }
        startActivity(Intent(this, MainActivity::class.java).addFlags(Intent.FLAG_ACTIVITY_CLEAR_TOP or Intent.FLAG_ACTIVITY_SINGLE_TOP))
        finish()
    }
}
