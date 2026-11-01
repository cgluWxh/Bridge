package com.bonevane.bridge

import android.app.Notification
import android.content.ComponentName
import android.content.Context
import android.os.Bundle
import android.provider.Settings
import android.service.notification.NotificationListenerService
import android.service.notification.StatusBarNotification
import java.util.concurrent.CopyOnWriteArrayList

/**
 * Mirrors the phone's notifications to the Mac.
 *
 * This is an ordinary Android API (a notification listener the user grants once
 * in Settings), not a privileged one, so it keeps working while USB debugging
 * is off: notifications arrive even when the phone is fully locked down.
 *
 * The text is handed to [NotificationRelay] as one line per notification, and
 * `ControlProxy` streams those lines to whoever asked for them.
 */
class NotificationService : NotificationListenerService() {

    companion object {
        /** The running listener, for replies, dismissals and media sessions. */
        @Volatile var current: NotificationService? = null
    }

    override fun onListenerConnected() {
        NotificationRelay.connected = true
        current = this
        TunnelState.log("Notification access granted")
        MediaRelay.start(this)
    }

    override fun onListenerDisconnected() {
        NotificationRelay.connected = false
        current = null
        MediaRelay.stop()
    }

    /** Gone on the phone (read, swiped, answered): gone on the computers too. */
    override fun onNotificationRemoved(sbn: StatusBarNotification) {
        NotificationRelay.removed(sbn.key)
    }

    /**
     * Replies through the notification's own Reply action, the same way a
     * watch or Android Auto does: fill its RemoteInput and fire its intent.
     * Returns why not, or null on success.
     */
    fun reply(key: String, text: String): String? {
        val sbn = runCatching { getActiveNotifications(arrayOf(key)) }.getOrNull()?.firstOrNull()
            ?: return "that notification is gone from the phone"
        val target = NotificationRelay.replyTarget(sbn.packageName, sbn.notification)
            ?: return "it has no reply action"
        val intent = android.content.Intent()
        val results = android.os.Bundle().apply {
            target.inputs.forEach { putCharSequence(it.resultKey, text) }
        }
        android.app.RemoteInput.addResultsToIntent(target.inputs, intent, results)
        return runCatching { target.intent.send(this, 0, intent); null }
            .getOrElse { "the app refused it (${it.message})" }
    }

    fun dismiss(key: String) {
        runCatching { cancelNotification(key) }
    }

    override fun onNotificationPosted(sbn: StatusBarNotification) {
        if (sbn.packageName == packageName) return          // never mirror our own
        val n = sbn.notification ?: return

        val app = runCatching {
            packageManager.getApplicationLabel(
                packageManager.getApplicationInfo(sbn.packageName, 0)
            ).toString()
        }.getOrDefault(sbn.packageName)
        // Remembered even when skipped, so the user can find it in the list.
        Prefs.rememberApp(this, sbn.packageName, app)

        NotificationFilter.reasonToSkip(this, this, sbn)?.let { reason ->
            TunnelState.log("Not mirrored ($reason): $app")
            return
        }

        val extras = n.extras
        val title = extras.getCharSequence(Notification.EXTRA_TITLE)?.toString().orEmpty()
        val fallbackText = (extras.getCharSequence(Notification.EXTRA_TEXT)
            ?: extras.getCharSequence(Notification.EXTRA_BIG_TEXT))?.toString().orEmpty()
        val text = NotificationRelay.mirroredText(sbn.packageName, n, fallbackText) ?: return
        if (title.isBlank() && text.isBlank()) return

        NotificationRelay.post(app, title, text, sbn.packageName, sbn.key,
            replyable = NotificationRelay.replyTarget(sbn.packageName, n) != null, clearable = sbn.isClearable)
    }
}

/** Shared fan-out point between the listener service and the tunnel. */
object NotificationRelay {
    @Volatile var connected = false

    private val subscribers = CopyOnWriteArrayList<(String) -> Unit>()
    private val removalSubscribers = CopyOnWriteArrayList<(Int) -> Unit>()

    /**
     * Short ids for notification keys (which run to 60+ characters, a lot for
     * Bluetooth), so a computer can say "reply 12 …" or "dismiss 12". The
     * same key keeps its id, which is also what lets a computer replace an
     * updated notification (a chat's next message) instead of stacking it.
     */
    private val idsByKey = object : LinkedHashMap<String, Int>(64, 0.75f, true) {
        override fun removeEldestEntry(eldest: MutableMap.MutableEntry<String, Int>) = size > 300
    }
    private val keysById = HashMap<Int, String>()
    private var nextId = 1

    @Synchronized private fun idFor(key: String): Int =
        idsByKey.getOrPut(key) { nextId++.also { keysById[it] = key } }.also {
            if (keysById.size > 400) keysById.keys.retainAll(idsByKey.values.toSet())
        }

    @Synchronized fun keyFor(id: Int): String? = keysById[id]

    private const val WECHAT_PACKAGE = "com.tencent.mm"

    /** Nevolution's marker on messages echoed back from our own replies. */
    private const val REPLY_MARKER = "nevo.replyId"

    /**
     * NevolutionXposed normalizes WeChat's history into EXTRA_MESSAGES and trims
     * already-read lines, so mirror that instead of the raw "[2条]" text. Its
     * inline image preview keeps the same key with a picture in the extras; the
     * computer gets the "[图片]" placeholder, never the bitmap.
     */
    fun mirroredText(packageName: String, n: Notification, fallback: String): String? {
        if (packageName != WECHAT_PACKAGE) return fallback
        val stored = n.extras.getParcelableArray(Notification.EXTRA_MESSAGES)
            ?: return if (imagePreview(n)) "[图片]" else fallback
        val messages = ArrayList<String>()
        for (item in stored) {
            val message = item as? Bundle ?: continue
            // Never mirror our own replies back to the computer.
            if (message.getBundle("extras")?.containsKey(REPLY_MARKER) == true) continue
            val text = message.getCharSequence("text")?.toString()
            if (!text.isNullOrBlank()) messages.add(text.trim())
        }
        // Echo-only repost: nothing new for the computer; its title is even "我".
        // Returning null drops the whole notification, title included.
        return if (messages.isNotEmpty()) messages.joinToString(" / ") else null
    }

    /** Nevolution's expanded image preview rides on the original notification. */
    private fun imagePreview(n: Notification): Boolean {
        val picture: android.os.Parcelable? = n.extras.getParcelable(Notification.EXTRA_PICTURE)
        return picture != null || n.extras.getBoolean("nevo.wechat.imageReady")
    }

    /** A usable reply endpoint obtained from a normal action or WeChat's car extension. */
    data class ReplyTarget(
        val intent: android.app.PendingIntent,
        val inputs: Array<android.app.RemoteInput>,
    )

    /**
     * The normal path uses a visible notification action. WeChat 8.0.72/3100
     * instead publishes its reply endpoint only in CarExtender; Bridge can send
     * that original PendingIntent directly, without rebuilding the notification.
     */
    fun replyTarget(packageName: String, n: Notification?): ReplyTarget? {
        val action = n?.actions?.firstOrNull { a ->
            a.actionIntent != null && a.remoteInputs?.any { it.allowFreeFormInput } == true
        }
        action?.remoteInputs?.let { return ReplyTarget(action.actionIntent, it) }

        if (packageName != WECHAT_PACKAGE || n == null) return null
        return runCatching {
            val conversation = Notification.CarExtender(n).unreadConversation ?: return null
            val intent = conversation.replyPendingIntent ?: return null
            val input = conversation.remoteInput ?: return null
            ReplyTarget(intent, arrayOf(input))
        }.getOrNull()
    }

    /**
     * One notification as a single line, tab separated: app, title, text,
     * package (for its icon), id (for reply/dismiss), flags ("r" = can be
     * replied to, "c" = can be cleared).
     */
    fun post(app: String, title: String, text: String, pkg: String = "", key: String? = null,
             replyable: Boolean = false, clearable: Boolean = true) {
        fun clean(s: String) = s.replace('\t', ' ').replace('\n', ' ').trim()
        val id = key?.let { idFor(it) } ?: 0
        val flags = (if (replyable) "r" else "") + (if (clearable) "c" else "")
        val line = "${clean(app)}\t${clean(title)}\t${clean(text)}\t${clean(pkg)}\t$id\t$flags"
        if (isDuplicateRelay(key, line)) return
        subscribers.forEach { runCatching { it(line) } }
    }

    /** Nevolution reposts a notification after its read cleanup, and Android can
     *  post the same key twice on ranking updates. Popping the identical toast
     *  again on the computers reads as a duplicate, so swallow exact repeats of
     *  the same key within a short window. A changed line always gets through. */
    private val lastRelayed = object : LinkedHashMap<String, Pair<String, Long>>(64, 0.75f, true) {
        override fun removeEldestEntry(eldest: MutableMap.MutableEntry<String, Pair<String, Long>>) = size > 400
    }

    @Synchronized private fun isDuplicateRelay(key: String?, line: String): Boolean {
        if (key == null) return false
        val now = android.os.SystemClock.elapsedRealtime()
        val previous = lastRelayed[key]
        val duplicate = previous != null && previous.first == line && now - previous.second <= 2000L
        lastRelayed[key] = line to now
        return duplicate
    }

    /** A mirrored notification left the phone: tell the computers its id. */
    fun removed(key: String) {
        val id = synchronized(this) { idsByKey[key] } ?: return
        removalSubscribers.forEach { runCatching { it(id) } }
    }

    fun subscribeRemovals(listener: (Int) -> Unit) { removalSubscribers.add(listener) }
    fun unsubscribeRemovals(listener: (Int) -> Unit) { removalSubscribers.remove(listener) }

    fun subscribe(listener: (String) -> Unit) { subscribers.add(listener) }
    fun unsubscribe(listener: (String) -> Unit) { subscribers.remove(listener) }

    /** Whether the user has granted notification access to Bridge. */
    fun hasAccess(ctx: Context): Boolean {
        val enabled = Settings.Secure.getString(ctx.contentResolver, "enabled_notification_listeners").orEmpty()
        val me = ComponentName(ctx, NotificationService::class.java).flattenToString()
        return enabled.split(':').any { it == me || it.endsWith(NotificationService::class.java.name) }
    }
}
