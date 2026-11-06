package com.bonevane.bridge

import android.content.Context
import android.provider.Settings

/**
 * While a computer is linked, notifications are mirrored there, so the phone
 * shouldn't light its screen for them. Honor's "wake screen on notification"
 * switch is Settings.Secure `wakeup_when_receive_notification`; this turns it
 * off on link and back on afterwards, only if it was on before. Needs
 * WRITE_SECURE_SETTINGS and the setting to exist; otherwise does nothing.
 */
object WakeOnNotification {
    private const val KEY = "wakeup_when_receive_notification"

    // True only while we are the ones who turned it off.
    private var turnedOff = false

    @Synchronized
    fun onLinkChanged(ctx: Context, linked: Boolean) {
        runCatching {
            if (ctx.checkSelfPermission(android.Manifest.permission.WRITE_SECURE_SETTINGS) !=
                android.content.pm.PackageManager.PERMISSION_GRANTED
            ) return
            val cr = ctx.contentResolver
            if (linked) {
                if (turnedOff) return
                val cur = Settings.Secure.getString(cr, KEY) ?: return // no such setting
                if (cur == "0") return // already off, leave it alone
                if (Settings.Secure.putInt(cr, KEY, 0)) turnedOff = true
            } else if (turnedOff) {
                turnedOff = false
                if (Settings.Secure.getString(cr, KEY) != null) Settings.Secure.putInt(cr, KEY, 1)
            }
        }
    }
}
