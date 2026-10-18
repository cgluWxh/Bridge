package com.bonevane.bridge

import android.content.Context
import android.net.wifi.WifiManager
import android.os.Handler
import android.os.Looper
import android.os.PowerManager
import android.os.SystemClock

/** Short BLE-triggered prewarm, handed over to authenticated business streams.
 * Merely listening, automatic status broadcasts and LAN STATUS probes hold no locks.
 * All lifecycle changes are serialized; consult the current stream count rather
 * than trusting a possibly out-of-order first/last-stream notification.
 */
class LanPower(
    context: Context,
) {
    private val power = context.getSystemService(PowerManager::class.java)
    private val wifi = context.applicationContext.getSystemService(WifiManager::class.java)
    private val handler = Handler(Looper.getMainLooper())
    private var closed = false
    private var warmUntil = 0L
    private var business = false
    private var warmCpu: PowerManager.WakeLock? = null
    private var warmWifi: WifiManager.WifiLock? = null
    private var businessCpu: PowerManager.WakeLock? = null
    private var businessWifi: WifiManager.WifiLock? = null
    private val expire = Runnable { expireWarmup() }

    private fun cpu(
        tag: String,
        timeoutMs: Long? = null,
    ): PowerManager.WakeLock? =
        runCatching {
            power.newWakeLock(PowerManager.PARTIAL_WAKE_LOCK, tag).apply {
                setReferenceCounted(false)
                if (timeoutMs == null) acquire() else acquire(timeoutMs)
            }
        }.onFailure { TunnelState.log("LAN CPU lock unavailable: ${it.message}") }.getOrNull()

    @Suppress("DEPRECATION")
    private fun wifi(tag: String): WifiManager.WifiLock? =
        runCatching {
            // LOW_LATENCY is conditional on foreground/screen state on Android 10.
            // HIGH_PERF is the better short-lived hint for screen-off connection.
            wifi.createWifiLock(WifiManager.WIFI_MODE_FULL_HIGH_PERF, tag).apply {
                setReferenceCounted(false)
                acquire()
            }
        }.onFailure { TunnelState.log("LAN Wi-Fi lock unavailable: ${it.message}") }.getOrNull()

    /** Only call after authenticating a peer's explicit BLE status command. */
    @Synchronized fun prewarm() {
        if (closed) return
        if (TunnelState.openStreams.get() > 0) {
            refreshBusiness()
            return // Already owned by business: no trailing warm lease needed.
        }
        warmUntil = SystemClock.elapsedRealtime() + 10_000
        // Recreate the timed CPU lock so an earlier acquire(timeout)'s release
        // cannot shorten a refreshed non-reference-counted lease.
        runCatching { warmCpu?.let { if (it.isHeld) it.release() } }
        warmCpu = cpu("Bridge::lan-prewarm", 10_000)
        if (warmWifi == null) warmWifi = wifi("Bridge::lan-prewarm-wifi")
        handler.removeCallbacks(expire)
        handler.postDelayed(expire, 10_000)
        TunnelState.log("LAN prewarm: explicit BLE status; CPU=${warmCpu?.isHeld == true} Wi-Fi=${warmWifi?.isHeld == true}, 10 s lease")
    }

    @Synchronized private fun expireWarmup() {
        if (closed || warmUntil == 0L) return
        val remaining = warmUntil - SystemClock.elapsedRealtime()
        if (remaining > 0) {
            handler.postDelayed(expire, remaining)
            return
        }
        releaseWarmup()
        TunnelState.log("LAN prewarm expired: no business takeover")
    }

    private fun releaseWarmup() {
        warmUntil = 0
        handler.removeCallbacks(expire)
        runCatching { warmCpu?.let { if (it.isHeld) it.release() } }
        runCatching { warmWifi?.let { if (it.isHeld) it.release() } }
        warmCpu = null
        warmWifi = null
    }

    @Synchronized fun refreshBusiness() {
        if (closed) return
        val streams = TunnelState.openStreams.get()
        if (streams > 0) {
            if (businessCpu == null) businessCpu = cpu("Bridge::business")
            if (businessWifi == null) businessWifi = wifi("Bridge::business-wifi")
            // Acquire business locks BEFORE releasing prewarm locks.
            releaseWarmup()
            if (!business) {
                TunnelState.log(
                    "LAN/session power: business takeover ($streams stream(s)); CPU=${businessCpu?.isHeld == true} Wi-Fi=${businessWifi?.isHeld == true}",
                )
            }
            business = true
        } else {
            runCatching { businessCpu?.let { if (it.isHeld) it.release() } }
            runCatching { businessWifi?.let { if (it.isHeld) it.release() } }
            businessCpu = null
            businessWifi = null
            if (business) TunnelState.log("LAN/session power: last business stream ended; locks released")
            business = false
        }
    }

    @Synchronized fun close() {
        closed = true
        releaseWarmup()
        runCatching { businessCpu?.let { if (it.isHeld) it.release() } }
        runCatching { businessWifi?.let { if (it.isHeld) it.release() } }
        businessCpu = null
        businessWifi = null
        business = false
    }
}
