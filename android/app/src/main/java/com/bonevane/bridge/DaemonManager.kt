package com.bonevane.bridge

import android.content.Context
import android.provider.Settings
import java.net.InetSocketAddress
import java.net.Socket

/**
 * Starts and stops [Daemon] via ADB (shell uid), or optionally su (uid 0).
 * Root mode does not require or change USB/wireless debugging.
 *
 * Precondition: this app holds WRITE_SECURE_SETTINGS (granted once over USB by
 * the Mac, or by an earlier daemon), so it can switch USB debugging on for the
 * session and off again afterwards.
 *
 * Steps: enable USB debugging (adbd must stay alive: ADB-spawned processes die
 * with it) → open the wireless-debugging door for a second → `app_process` the
 * daemon → wait for it on 5577. adbd never listens on TCP.
 */
object DaemonManager {
    const val DAEMON_PORT = 5577

    /** Liveness only: a listening port is enough, no secret needed to ask. */
    fun isDaemonAlive(): Boolean = probe(DAEMON_PORT) != null

    /** QUIT ends sessions too: their scrcpy children must not outlive the helper. */
    private fun quitDaemon(ctx: Context) {
        Socket("127.0.0.1", DAEMON_PORT).use { s ->
            s.soTimeout = 3000
            s.getOutputStream().write((Pairing.authLine(ctx) + "\nQUIT\n").toByteArray())
            s.getOutputStream().flush()
            val input = s.getInputStream().bufferedReader()
            check(input.readLine()?.startsWith("bridge-daemon") == true) { "Helper authentication failed" }
            check(input.readLine() == "OK bye") { "Helper refused to stop" }
        }
        repeat(20) {
            if (!portOpen(DAEMON_PORT)) return
            Thread.sleep(100)
        }
        error("Helper did not stop")
    }

    private fun startRoot(ctx: Context) {
        check(!portOpen(DAEMON_PORT)) { "Another helper holds port $DAEMON_PORT; stop it first" }

        fun quote(value: String) = "'" + value.replace("'", "'\\''") + "'"
        val cmd =
            "test \"$(id -u)\" = 0 || exit 1; " +
                "(BRIDGE_SECRET=${quote(Prefs.pairSecret(ctx))} CLASSPATH=${quote(ctx.applicationInfo.sourceDir)} " +
                "exec setsid app_process / com.bonevane.bridge.Daemon " +
                "</dev/null >/data/local/tmp/bridge-daemon-root.out 2>&1) & sleep 1"
        val process = ProcessBuilder("su", "-c", cmd).redirectErrorStream(true).start()
        try {
            check(process.waitFor(60, java.util.concurrent.TimeUnit.SECONDS)) { "Root authorization timed out" }
            check(process.exitValue() == 0) { "Root authorization or helper launch failed" }
        } finally {
            process.destroy()
        }
        repeat(20) {
            val hello = probe(DAEMON_PORT)
            if (hello != null && hello.contains("uid=0 ")) {
                TunnelState.log("Root helper up: $hello")
                TunnelService.current?.ble?.sendStatus()
                return
            }
            Thread.sleep(250)
        }
        error("Root helper did not answer; see /data/local/tmp/bridge-daemon-root.out")
    }

    private fun portOpen(port: Int): Boolean =
        runCatching {
            Socket().use { it.connect(InetSocketAddress("127.0.0.1", port), 300) }
            true
        }.getOrDefault(false)

    /**
     * Returns the daemon's hello line ("bridge-daemon uid=2000 pid=… up=…s") or
     * null. The hello only comes after the pairing secret, so a stranger who
     * finds the port learns nothing, not even that it's Bridge.
     */
    fun probe(
        port: Int,
        timeoutMs: Int = 500,
    ): String? =
        runCatching {
            Socket().use { s ->
                s.connect(InetSocketAddress("127.0.0.1", port), timeoutMs)
                s.soTimeout = timeoutMs
                s.getOutputStream().write((Pairing.authLine(AppContext.value) + "\n").toByteArray())
                s.getOutputStream().flush()
                s
                    .getInputStream()
                    .bufferedReader()
                    .readLine()
                    ?.takeIf { it.startsWith("bridge-daemon") }
            }
        }.getOrNull()

    /** The phone's "Stop mirroring": asks the helper to end every screen session. Blocking. */
    fun endSessions(): String =
        runCatching {
            Socket("127.0.0.1", DAEMON_PORT).use { s ->
                s.soTimeout = 3000
                s.getOutputStream().write((Pairing.authLine(AppContext.value) + "\nENDSESSIONS\n").toByteArray())
                s.getOutputStream().flush()
                val input = s.getInputStream().bufferedReader()
                input.readLine() // hello
                input.readLine() ?: "no reply"
            }
        }.getOrElse { "couldn't reach the helper: ${it.message}" }

    /** Credentials travel only to the requesting BLE peer. Never log the return value. */
    fun startHotspot(ctx: Context): String {
        if (!Prefs.rootHelper(ctx)) return "ERR\troot_required"
        if (Prefs.pausedUntil(ctx) > System.currentTimeMillis()) return "ERR\tphone_paused"
        return runCatching {
            start(ctx) // Root path only; does not turn on ADB.
            Socket().use { s ->
                s.connect(InetSocketAddress("127.0.0.1", DAEMON_PORT), 1000)
                s.soTimeout = 25_000
                s.getOutputStream().write((Pairing.authLine(ctx) + "\nHOTSPOT\n").toByteArray())
                s.getOutputStream().flush()
                val input = s.getInputStream().bufferedReader()
                val hello = input.readLine()
                check(hello?.startsWith("bridge-daemon uid=0 ") == true)
                input.readLine()?.takeIf { it.startsWith("OK\t") || it.startsWith("ERR\t") }
                    ?: "ERR\thelper_protocol"
            }
        }.getOrElse { "ERR\troot_helper_unavailable" }
    }

    /** Blocking; call from a background thread. Throws with a readable message on failure. */
    @Synchronized
    fun start(ctx: Context) {
        // On Android 10 only the computer may start/replace the helper.
        // Do not stop an existing helper or change ADB settings here.
        if (LegacyAndroid.manualHelper) {
            if (isDaemonAlive()) {
                LegacyAndroid.activated(ctx)
                return
            }
            LegacyAndroid.requireUsb(ctx)
        }
        val squatter = probe(DAEMON_PORT) == null && portOpen(DAEMON_PORT)
        probe(DAEMON_PORT)?.let { hello ->
            // A daemon from before an app update still runs the old code from a
            // directory that no longer exists; replace it.
            val apk = hello.substringAfter("apk=", "")
            if (apk == ctx.applicationInfo.sourceDir &&
                (hello.contains("uid=0 ") == Prefs.rootHelper(ctx))
            ) {
                TunnelState.log("Daemon already running")
                return
            }
            quitDaemon(ctx)
            TunnelState.log("Restarting helper for the current install and privilege mode")
        }

        if (Prefs.rootHelper(ctx)) {
            startRoot(ctx)
            return
        }

        if (!AdbToggle.isGranted(ctx)) {
            error("WRITE_SECURE_SETTINGS not granted yet; run \"Set up over USB\" once")
        }
        // Something holds the port but doesn't know our secret: a helper left
        // over from a reinstall (the app's data went, the shell process didn't).
        // It can't be told to quit, and a new helper can't bind while it's
        // there, so every start would fail forever. Restarting USB debugging
        // kills adbd's whole process group, the squatter included.
        if (squatter && AdbToggle.isEnabled(ctx)) {
            TunnelState.log("A helper that doesn't know the pairing secret holds port $DAEMON_PORT; clearing it")
            AdbToggle.set(ctx, false)
            for (i in 1..20) {
                if (!portOpen(DAEMON_PORT)) break
                Thread.sleep(250)
            }
            Thread.sleep(500)
        }
        if (!AdbToggle.isEnabled(ctx)) {
            AdbToggle.set(ctx, true)
        }

        // Safety net: if adbd is in TCP mode (a leftover `adb tcpip` from before a
        // reboot clears it), switch it back to USB-only first. This restarts adbd,
        // so it has to happen before we spawn anything.
        if (probe(5555, timeoutMs = 300) != null || portOpen(5555)) {
            TunnelState.log("adbd is listening on TCP 5555; switching it back to USB mode")
            WifiBootstrap.withAdb(ctx) { it.service("usb:") }
            Thread.sleep(2000)
        }

        WifiBootstrap.withAdb(ctx) {
            TunnelState.log("Connected to adbd")
            // Keep the permission fresh (idempotent), then spawn the daemon detached.
            it.shell("pm grant ${ctx.packageName} android.permission.WRITE_SECURE_SETTINGS")
            val apk = ctx.applicationInfo.sourceDir
            // The legacy `shell:` service gives us a PTY that adbd hangs up when the
            // shell exits, so the daemon must be in its own session (setsid), not
            // attached to the PTY (</dev/null), and the shell waits 1 s for that.
            // The secret rides in the environment, which only the daemon's own
            // uid can read; the command line would be visible more widely.
            val cmd =
                "(BRIDGE_SECRET=${Prefs.pairSecret(ctx)} CLASSPATH=$apk exec setsid app_process / com.bonevane.bridge.Daemon " +
                    "</dev/null >/data/local/tmp/bridge-daemon.out 2>&1) & sleep 1"
            it.shell(cmd)
        }

        for (attempt in 1..20) {
            probe(DAEMON_PORT)?.let {
                TunnelState.log("Daemon up: $it")
                TunnelService.current?.ble?.sendStatus()
                return
            }
            Thread.sleep(250)
        }
        error("daemon did not answer on $DAEMON_PORT")
    }

    /**
     * Turns USB debugging off, which takes the daemon down with it (init kills
     * adbd's whole cgroup). This is the "lock down" state banking apps want.
     *
     * Exception, agreed with Bone: on cellular (no Wi-Fi) the daemon stays up,
     * because starting it again needs wireless debugging, which needs Wi-Fi;
     * locking down there would mean no way back in until the next Wi-Fi.
     * Returns a one-line description of what happened.
     */
    @Synchronized
    fun stop(
        ctx: Context,
        force: Boolean = false,
    ): String {
        if (!force && Prefs.keepReady(ctx)) {
            TunnelState.log("Disconnect: keeping the daemon (\"keep ready\" is on)")
            return "kept ready (setting)"
        }
        val root = probe(DAEMON_PORT)?.contains("uid=0 ") == true
        if (!force && !root && !Prefs.rootHelper(ctx) && !onWifi(ctx)) {
            TunnelState.log("Disconnect on cellular: keeping the daemon (no Wi-Fi to restart it)")
            return "kept ready: phone is on cellular"
        }
        if (isDaemonAlive()) quitDaemon(ctx)
        // Root mode never changes the user's ADB setting.
        if (!root && !Prefs.rootHelper(ctx)) AdbToggle.set(ctx, false)
        TunnelState.log("Daemon stopped")
        TunnelService.current?.ble?.sendStatus()
        return if (root || Prefs.rootHelper(ctx)) "helper stopped (ADB unchanged)" else "stopped, USB debugging off"
    }

    fun onWifi(ctx: Context): Boolean {
        val cm = ctx.getSystemService(android.net.ConnectivityManager::class.java)
        val caps = cm.getNetworkCapabilities(cm.activeNetwork) ?: return false
        return caps.hasTransport(android.net.NetworkCapabilities.TRANSPORT_WIFI)
    }
}
