package com.bonevane.bridge

import android.app.Notification
import android.app.NotificationManager
import android.content.Context
import android.service.notification.NotificationListenerService
import android.provider.Settings
import android.service.notification.StatusBarNotification

/**
 * Decides which of the phone's notifications are worth showing on the Mac.
 *
 * Three filters, all off-by-default-safe:
 *  - **silent** ones (low importance, group summaries, ongoing): Android didn't
 *    make a sound or peek for them, so the Mac shouldn't either;
 *  - **everything not whitelisted** while Do Not Disturb is on;
 *  - **apps the user switched off** in the list on the phone screen;
 *  - **apps that are open on the Mac right now**: WhatsApp on the Mac will
 *    show its own banner, so the phone's copy would be a duplicate. The Mac
 *    reports which twins are running (as Android package names) over Bluetooth.
 */
object NotificationFilter {

    /** Android packages whose Mac counterpart is currently running. */
    @Volatile var openOnMac: Set<String> = emptySet()
        private set

    /** Called by [BleLink] with the Mac's "macapps …" message. */
    fun setOpenOnMac(packages: Collection<String>) {
        val next = packages.toSet()
        if (next != openOnMac) {
            openOnMac = next
            TunnelState.log(if (next.isEmpty()) "Mac reports no twin apps open"
                            else "Open on the Mac: ${next.joinToString(", ")}")
            TunnelState.notifyListeners()
        }
    }

    /** Why a notification was dropped, or null to relay it. */
    fun reasonToSkip(ctx: Context, service: NotificationListenerService, sbn: StatusBarNotification): String? {
        val n = sbn.notification ?: return "empty"
        if (n.flags and Notification.FLAG_ONGOING_EVENT != 0) return "ongoing"
        if (n.flags and Notification.FLAG_GROUP_SUMMARY != 0) return "group summary"

        if (blockedByDnd(ctx, sbn)) return "do not disturb"
        if (Prefs.skipSilent(ctx) && isSilent(service, sbn)) return "silent"
        if (!Prefs.appMirrored(ctx, sbn.packageName)) return "switched off"
        if (Prefs.skipOpenOnMac(ctx) && sbn.packageName in openOnMac) return "open on the Mac"
        return null
    }

    private val dndEssentialPackages = setOf(
        "android", "com.android.systemui", "com.android.phone", "com.android.server.telecom",
        "com.android.incallui", "com.hihonor.incallui",
    )

    /**
     * While the phone's Do Not Disturb is on, only apps on the HnHookUtils whitelist
     * (Settings.Secure "hn_dnd_whitelist", comma separated) are relayed. Calls, alarms
     * and system packages are left alone, matching what the module does on the phone.
     */
    private fun blockedByDnd(ctx: Context, sbn: StatusBarNotification): Boolean {
        val zen = runCatching { Settings.Global.getInt(ctx.contentResolver, "zen_mode", 0) }.getOrDefault(0)
        if (zen == 0) return false
        val pkg = sbn.packageName
        if (pkg in dndEssentialPackages) return false
        val category = sbn.notification?.category
        if (category == Notification.CATEGORY_CALL || category == Notification.CATEGORY_ALARM) return false
        val raw = runCatching { Settings.Secure.getString(ctx.contentResolver, "hn_dnd_whitelist") }.getOrNull()
        val whitelist = raw.orEmpty().split(',').map { it.trim() }.filter { it.isNotEmpty() }
        return pkg !in whitelist
    }

    /**
     * "Silent" the way the phone itself treats it: the notification's ranking
     * (channel importance after the user's per-app choices) is below DEFAULT,
     * which is the line under which Android neither sounds nor peeks.
     */
    private fun isSilent(service: NotificationListenerService, sbn: StatusBarNotification): Boolean {
        val ranking = NotificationListenerService.Ranking()
        val known = runCatching { service.currentRanking.getRanking(sbn.key, ranking) }.getOrDefault(false)
        if (known) return ranking.importance < NotificationManager.IMPORTANCE_DEFAULT || ranking.isAmbient
        // No ranking (shouldn't happen): fall back to the app's own priority hint.
        @Suppress("DEPRECATION")
        return sbn.notification.priority < Notification.PRIORITY_DEFAULT
    }
}
