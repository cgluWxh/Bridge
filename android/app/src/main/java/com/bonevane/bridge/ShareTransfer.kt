package com.bonevane.bridge

import android.app.*
import android.content.Intent
import android.content.pm.ServiceInfo
import android.net.Uri
import android.os.*
import android.provider.OpenableColumns
import java.io.File
import java.net.Socket
import java.util.UUID
import java.util.concurrent.CountDownLatch
import java.util.concurrent.TimeUnit
import java.util.concurrent.atomic.AtomicBoolean

/** One foreground, user-initiated batch. No URI or capability is written to logs. */
object ShareTransfer {
    data class Item(
        val name: String,
        val state: String = "Queued",
        val size: Long = 0,
        val sent: Long = 0,
    )

    data class Snapshot(
        val active: Boolean,
        val peer: String,
        val phase: String,
        val items: List<Item>,
    ) {
        val succeeded get() = items.count { it.state == "Sent" }
        val failed get() = items.count { it.state.startsWith("Failed:") }
        val summary get() = "$succeeded succeeded · $failed failed · ${items.size} files"
    }

    @Volatile var snapshot = Snapshot(false, "", "No transfer", emptyList())
        private set

    @Volatile private var service: ShareService? = null
    val active get() = snapshot.active

    @Synchronized fun attach(
        s: ShareService,
        peer: String,
        count: Int,
    ): Boolean {
        if (active) return false
        service = s
        snapshot = Snapshot(true, peer, "Preparing files…", List(count) { Item("File ${it + 1}") })
        return true
    }

    @Synchronized fun phase(
        owner: ShareService,
        text: String,
    ) {
        if (service === owner) snapshot = snapshot.copy(phase = text)
    }

    @Synchronized fun item(
        owner: ShareService,
        index: Int,
        value: Item,
    ) {
        if (service === owner) snapshot = snapshot.copy(items = snapshot.items.toMutableList().also { it[index] = value })
    }

    @Synchronized fun finish(
        owner: ShareService,
        reason: String,
    ) {
        if (service !== owner) return
        snapshot =
            snapshot.copy(
                active = false,
                phase = reason,
                items =
                    snapshot.items.map {
                        if (it.state == "Sent" || it.state.startsWith("Failed:")) {
                            it
                        } else {
                            it.copy(state = "Failed: ${if (reason == "Finished") "Not confirmed by computer" else reason}")
                        }
                    },
            )
        service = null
    }

    fun cancel() {
        service?.cancel()
    }

    fun serve(
        socket: Socket,
        token: String,
    ) {
        val s = service
        if (s == null || !s.accept(socket, token)) {
            socket.getOutputStream().write("ERR share unavailable\n".toByteArray())
        }
    }
}

/** Keeps URI grants, staging and transfer alive after the share Activity leaves. */
class ShareService : Service() {
    companion object {
        const val CHANNEL = "file-transfers"
        const val NOTIFICATION = 42
    }

    private val main = Handler(Looper.getMainLooper())
    private val claimed = AtomicBoolean(false)
    private val completed = CountDownLatch(1)

    @Volatile private var cancelled = false

    @Volatile private var socket: Socket? = null

    @Volatile private var token = ""

    @Volatile private var lastProgress = 0L

    @Volatile private var transferError: String? = null

    @Volatile private var source: java.io.InputStream? = null
    private var worker: Thread? = null
    private var staging: File? = null
    private val files = mutableListOf<File?>()
    private val tick =
        object : Runnable {
            override fun run() {
                if (ShareTransfer.active) {
                    getSystemService(NotificationManager::class.java).notify(NOTIFICATION, notification())
                    main.postDelayed(this, 750)
                }
            }
        }

    override fun onBind(intent: Intent?) = null

    override fun onStartCommand(
        intent: Intent?,
        flags: Int,
        startId: Int,
    ): Int {
        if (worker != null) return START_NOT_STICKY
        @Suppress("DEPRECATION")
        val uris = intent?.getParcelableArrayListExtra<Uri>("uris").orEmpty()
        if (uris.isEmpty() || !ShareTransfer.attach(this, intent?.getStringExtra("peerName").orEmpty(), uris.size)) {
            stopSelf()
            return START_NOT_STICKY
        }
        getSystemService(
            NotificationManager::class.java,
        ).createNotificationChannel(NotificationChannel(CHANNEL, "File transfers", NotificationManager.IMPORTANCE_LOW))
        if (Build.VERSION.SDK_INT >= 34) {
            startForeground(NOTIFICATION, notification(), ServiceInfo.FOREGROUND_SERVICE_TYPE_DATA_SYNC)
        } else {
            startForeground(NOTIFICATION, notification())
        }
        main.post(tick)
        val peer = intent?.getStringExtra("peer").orEmpty()
        worker = Thread({ runBatch(uris, peer) }, "share-files").also { it.start() }
        return START_NOT_STICKY // URI grants and in-memory tasks cannot survive process death.
    }

    private fun notification(): Notification {
        val state = ShareTransfer.snapshot
        val total = state.items.sumOf { it.size }
        val done = state.items.sumOf { it.sent }
        val open =
            PendingIntent.getActivity(
                this,
                42,
                Intent(this, ShareActivity::class.java),
                PendingIntent.FLAG_UPDATE_CURRENT or PendingIntent.FLAG_IMMUTABLE,
            )
        val builder =
            Notification
                .Builder(this, CHANNEL)
                .setSmallIcon(R.drawable.ic_stat_bridge)
                .setContentTitle(if (state.active) "Sharing to ${state.peer}" else "File sharing finished")
                .setContentText("${state.phase} · ${state.summary}")
                .setStyle(Notification.BigTextStyle().bigText("${state.phase}\n${state.summary}"))
                .setContentIntent(open)
                .setOnlyAlertOnce(true)
                .setOngoing(state.active)
        if (state.active) builder.setProgress(1000, if (total == 0L) 0 else (done.toDouble() / total * 1000).toInt(), total == 0L)
        return builder.build()
    }

    private fun runBatch(
        uris: List<Uri>,
        peer: String,
    ) {
        var result = "Finished"
        val wake =
            getSystemService(PowerManager::class.java)
                .newWakeLock(PowerManager.PARTIAL_WAKE_LOCK, "Bridge::share")
        wake.acquire(6 * 60 * 60 * 1000L)
        try {
            // A killed process cannot resume its URI grants. Reclaim its old staging.
            cacheDir
                .listFiles()
                ?.filter { it.isDirectory && it.name.startsWith("share-") }
                ?.forEach { it.deleteRecursively() }
            // Staging establishes exact lengths even for cloud providers, and avoids
            // keeping another app's transient URI grant throughout a network transfer.
            staging = File(cacheDir, "share-${UUID.randomUUID()}").also { check(it.mkdirs()) }
            uris.forEachIndexed { index, uri ->
                check(!cancelled) { "Cancelled" }
                var name = "File ${index + 1}"
                val file = File(staging, index.toString())
                try {
                    require(uri.scheme == "content") { "Only content URIs are supported" }
                    contentResolver.query(uri, arrayOf(OpenableColumns.DISPLAY_NAME), null, null, null)?.use {
                        if (it.moveToFirst()) name = it.getString(0)?.take(240)?.ifBlank { name } ?: name
                    }
                    ShareTransfer.phase(this, "Preparing ${index + 1}/${uris.size}: $name")
                    ShareTransfer.item(this, index, ShareTransfer.Item(name, "Preparing"))
                    contentResolver.openInputStream(uri)?.use { input ->
                        source = input
                        file.outputStream().use { out ->
                            val buf = ByteArray(128 * 1024)
                            while (true) {
                                check(!cancelled) { "Cancelled" }
                                val n = input.read(buf)
                                if (n < 0) break
                                out.write(buf, 0, n)
                            }
                        }
                    } ?: error("Source is not readable")
                    source = null
                    files.add(file)
                    ShareTransfer.item(this, index, ShareTransfer.Item(name, "Ready", file.length()))
                } catch (e: Exception) {
                    source = null
                    file.delete()
                    files.add(null)
                    ShareTransfer.item(this, index, ShareTransfer.Item(name, "Failed: ${e.message?.take(160)}"))
                }
            }
            check(!cancelled) { "Cancelled" }
            if (files.any { it != null }) {
                token = UUID.randomUUID().toString().replace("-", "")
                TunnelService.setTunnel(this, true, remember = false)
                ShareTransfer.phase(this, "Waiting for computer…")
                // A directed, encrypted BLE offer; only the selected peer gets the capability.
                lastProgress = SystemClock.elapsedRealtime()
                check(TunnelService.current?.ble?.offerShare(peer, token) == true) { "Computer disconnected" }
                // Timeout on *inactivity*, not total duration: large files may take hours.
                while (!completed.await(5, TimeUnit.SECONDS)) {
                    check(!cancelled) { "Cancelled" }
                    check(SystemClock.elapsedRealtime() - lastProgress < 180_000) { "Computer stopped responding" }
                }
                check(!cancelled) { "Cancelled" }
                transferError?.let { error(it) }
            }
        } catch (e: Exception) {
            result = if (cancelled) "Cancelled" else e.message?.take(160) ?: "Transfer interrupted"
        } finally {
            cancelled = true
            token = ""
            runCatching { socket?.close() }
            // Wait for the stream thread before publishing the final report, so
            // it cannot overwrite a new batch's state after cancellation.
            Thread.interrupted()
            runCatching { if (claimed.get()) completed.await(5, TimeUnit.SECONDS) }
            if (wake.isHeld) wake.release()
            staging?.deleteRecursively()
            main.post {
                ShareTransfer.finish(this, result)
                main.removeCallbacks(tick)
                stopForeground(STOP_FOREGROUND_DETACH)
                getSystemService(NotificationManager::class.java).notify(NOTIFICATION, notification())
                stopSelf()
                // A mirror may have ended while sharing held its tunnel open.
                // Re-run normal idle cleanup so its shell helper isn't left running.
                TunnelService.endSession(this, "File sharing")
            }
        }
    }

    /** SHAR <capability> -> OK <count>, then FILE <bytes> <hex UTF-8 name> + bytes.
     * Receiver sends READY (or ERR) before bytes, then OK only after safe commit.
     * Each preparation error occupies an ERR row; DONE ends the batch.
     */
    fun accept(
        client: Socket,
        supplied: String,
    ): Boolean {
        if (cancelled || token.isEmpty() || supplied != token || !claimed.compareAndSet(false, true)) return false
        socket = client
        lastProgress = SystemClock.elapsedRealtime()
        client.soTimeout = 60_000
        if (TunnelState.openStreams.incrementAndGet() == 1) TunnelService.current?.holdWifiAwake(true)
        try {
            val output = client.getOutputStream()
            val input = client.getInputStream()

            fun line(s: String) {
                output.write("$s\n".toByteArray())
                output.flush()
            }

            fun reply(): String {
                val bytes = java.io.ByteArrayOutputStream()
                while (bytes.size() < 4096) {
                    val b = input.read()
                    check(b >= 0) { "Computer disconnected" }
                    if (b == 10) return bytes.toString("UTF-8")
                    bytes.write(b)
                }
                error("Reply too long")
            }
            line("OK ${files.size}")
            files.forEachIndexed { index, file ->
                check(!cancelled) { "Cancelled" }
                val item = ShareTransfer.snapshot.items[index]
                if (file == null) {
                    line("ERR source-unavailable")
                    return@forEachIndexed
                }
                ShareTransfer.phase(this, "Sending ${index + 1}/${files.size}: ${item.name}")
                line("FILE ${item.size} ${item.name.toByteArray(Charsets.UTF_8).joinToString("") { "%02x".format(it.toInt() and 255) }}")
                if (reply() != "READY") {
                    ShareTransfer.item(this, index, item.copy(state = "Failed: Computer could not create file"))
                    return@forEachIndexed
                }
                var sent = 0L
                var last = 0L
                file.inputStream().use { source ->
                    val buf = ByteArray(128 * 1024)
                    while (sent < item.size) {
                        check(!cancelled) { "Cancelled" }
                        val n = source.read(buf, 0, minOf(buf.size.toLong(), item.size - sent).toInt())
                        check(n > 0) { "Source ended early" }
                        output.write(buf, 0, n)
                        sent += n
                        lastProgress = SystemClock.elapsedRealtime()
                        if (SystemClock.elapsedRealtime() - last > 250) {
                            ShareTransfer.item(this, index, item.copy(state = "Sending", sent = sent))
                            last = SystemClock.elapsedRealtime()
                        }
                    }
                    output.flush()
                }
                val ack = reply()
                ShareTransfer.item(
                    this,
                    index,
                    item.copy(
                        state =
                            if (ack ==
                                "OK"
                            ) {
                                "Sent"
                            } else {
                                "Failed: Computer could not save file"
                            },
                        sent = sent,
                    ),
                )
                file.delete()
            }
            line("DONE")
        } catch (e: Exception) {
            transferError = e.message?.take(160) ?: "Connection interrupted"
            ShareTransfer.phase(this, transferError!!)
            // Only confirmed files count as success; all other rows are finalized below.
        } finally {
            socket = null
            if (TunnelState.openStreams.decrementAndGet() == 0) TunnelService.current?.holdWifiAwake(false)
            completed.countDown()
        }
        return true
    }

    fun cancel() {
        cancelled = true
        runCatching { socket?.close() }
        runCatching { source?.close() }
        worker?.interrupt()
    }

    override fun onTimeout(
        startId: Int,
        fgsType: Int,
    ) {
        cancel()
        stopSelf()
    }

    override fun onDestroy() {
        if (ShareTransfer.active) cancel()
        main.removeCallbacks(tick)
        super.onDestroy()
    }
}
