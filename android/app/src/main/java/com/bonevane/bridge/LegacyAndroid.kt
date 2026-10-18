package com.bonevane.bridge

import android.app.Notification
import android.app.NotificationChannel
import android.app.NotificationManager
import android.app.PendingIntent
import android.content.Context
import android.content.Intent
import android.os.Build

/** Android 10 cannot bootstrap the helper through wireless debugging. */
object LegacyAndroid {
    val manualHelper: Boolean get() = Build.VERSION.SDK_INT < 30
    const val USB_REQUIRED = "Connect to your computer and use Set up over USB to activate the helper."
    private const val CHANNEL = "usb_activation"
    private const val ID = 1029
    private var notified = false

    @Synchronized
    fun requireUsb(ctx: Context): Nothing {
        if (!notified) {
            val nm = ctx.getSystemService(NotificationManager::class.java)
            nm.createNotificationChannel(NotificationChannel(CHANNEL, "USB activation", NotificationManager.IMPORTANCE_DEFAULT))
            val open = PendingIntent.getActivity(ctx, ID, Intent(ctx, MainActivity::class.java), PendingIntent.FLAG_IMMUTABLE)
            nm.notify(
                ID,
                Notification
                    .Builder(ctx, CHANNEL)
                    .setSmallIcon(R.drawable.ic_stat_bridge)
                    .setContentTitle("Helper needs USB activation")
                    .setContentText(USB_REQUIRED)
                    .setStyle(Notification.BigTextStyle().bigText(USB_REQUIRED))
                    .setContentIntent(open)
                    .setAutoCancel(true)
                    .setOnlyAlertOnce(true)
                    .build(),
            )
            notified = true
        }
        error(USB_REQUIRED)
    }

    @Synchronized
    fun activated(ctx: Context) {
        if (!manualHelper) return
        ctx.getSystemService(NotificationManager::class.java).cancel(ID)
        notified = false
    }
}
