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
    ) {
        val fraction get() = ShareTransfer.fraction(sent, size, state == "Sent" || state == "Saving")
    }

    fun fraction(
        done: Long,
        total: Long,
        emptyComplete: Boolean = false,
    ): Double =
        if (total > 0) {
            (done.toDouble() / total).coerceIn(0.0, 1.0)
        } else if (emptyComplete) {
            1.0
        } else {
            0.0
        }

    fun humanBytes(bytes: Double): String {
        val units = arrayOf("B", "KB", "MB", "GB", "TB")
        var value = if (bytes.isFinite()) bytes.coerceAtLeast(0.0) else 0.0
        var unit = 0
        while (value >= 1000 && unit < units.lastIndex) {
            value /= 1000
            unit++
        }
        return String.format(java.util.Locale.getDefault(), if (unit == 0) "%.0f %s" else "%.1f %s", value, units[unit])
    }

    fun percent(fraction: Double): String = String.format(java.util.Locale.getDefault(), "%.1f%%", fraction.coerceIn(0.0, 1.0) * 100)

    data class Snapshot(
        val active: Boolean,
        val peer: String,
        val phase: String,
        val items: List<Item>,
        val bytesPerSecond: Double = 0.0,
        val speedUpdatedAt: Long = 0,
    ) {
        val prepared get() = items.isNotEmpty() && items.none { it.state == "Queued" || it.state == "Preparing" }
        val totalBytes get() = items.sumOf { it.size }
        val sentBytes get() = items.sumOf { it.sent }
        val overallFraction get() =
            if (totalBytes > 0) {
                fraction(sentBytes, totalBytes)
            } else if (items.isEmpty()) {
                0.0
            } else {
                items.count { it.state == "Sent" || it.state == "Saving" }.toDouble() / items.size
            }
        val overallProgress get() = "${humanBytes(
            sentBytes.toDouble(),
        )} / ${humanBytes(totalBytes.toDouble())} · ${percent(overallFraction)}"

        // A recent sampling interval, not the average since the batch began.
        // Stop showing stale speed if the connection stalls or waits for a disk ACK.
        val currentBytesPerSecond get() = if (active && SystemClock.elapsedRealtime() - speedUpdatedAt < 1500) bytesPerSecond else 0.0
        val currentItem get() = items.firstOrNull { it.state == "Sending" || it.state == "Saving" }
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
        bytesPerSecond: Double? = null,
    ) {
        if (service === owner) {
            snapshot =
                snapshot.copy(
                    items = snapshot.items.toMutableList().also { it[index] = value },
                    bytesPerSecond = bytesPerSecond ?: snapshot.bytesPerSecond,
                    speedUpdatedAt = if (bytesPerSecond != null) SystemClock.elapsedRealtime() else snapshot.speedUpdatedAt,
                )
        }
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
                bytesPerSecond = 0.0,
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
        val progress =
            buildString {
                append(state.phase)
                if (state.prepared) {
                    append("\nOverall: ${state.overallProgress} · ${ShareTransfer.humanBytes(state.currentBytesPerSecond)}/s")
                    state.currentItem?.let { append("\nFile: ${ShareTransfer.percent(it.fraction)}") }
                }
                append("\n${state.summary}")
            }
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
                .setContentText(
                    if (state.prepared) {
                        "${state.overallProgress} · ${ShareTransfer.humanBytes(
                            state.currentBytesPerSecond,
                        )}/s"
                    } else {
                        state.phase
                    },
                ).setStyle(Notification.BigTextStyle().bigText(progress))
                .setContentIntent(open)
                .setOnlyAlertOnce(true)
                .setOngoing(state.active)
        if (state.active) builder.setProgress(1000, (state.overallFraction * 1000).toInt(), !state.prepared)
        return builder.build()
    }

    /** Legacy file:// intents must not turn this exported activity into a reader
     * of Bridge's own private files. Prefer caller-granted content:// URIs.
     */
    private fun localSharedFile(uri: Uri): File? {
        if (uri.scheme == "content") return null
        require(uri.scheme == "file") { "Only file or content URIs are supported" }
        val file = File(requireNotNull(uri.path) { "Missing file path" }).canonicalFile
        val roots = getSystemService(android.os.storage.StorageManager::class.java)
            .storageVolumes.mapNotNull { it.directory?.canonicalFile }
        require(roots.any { root ->
            val prefix = root.path + File.separator
            file.path.startsWith(prefix) &&
                !file.path.startsWith(prefix + "Android/data/") &&
                !file.path.startsWith(prefix + "Android/obb/")
        }) { "Use a content URI with read permission for this file" }
        require(file.isFile && file.canRead()) { "File is not readable; ask the file manager to grant read access" }
        return file
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
                    val local = localSharedFile(uri)
                    if (local != null) {
                        name = local.name.take(240).ifBlank { name }
                    } else {
                        contentResolver.query(uri, arrayOf(OpenableColumns.DISPLAY_NAME), null, null, null)?.use {
                            if (it.moveToFirst()) name = it.getString(0)?.take(240)?.ifBlank { name } ?: name
                        }
                    }
                    ShareTransfer.phase(this, "Preparing ${index + 1}/${uris.size}: $name")
                    ShareTransfer.item(this, index, ShareTransfer.Item(name, "Preparing"))
                    (local?.inputStream() ?: contentResolver.openInputStream(uri))?.use { input ->
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
                val ble = TunnelService.current?.ble
                // Updated clients request iroh only after LAN discovery fails.
                // Older clients still expect the phone to start iroh before the offer.
                if (ble?.sharePrefersLan(peer) != true) TunnelService.setTunnel(this, true, remember = false)
                ble?.sendStatus()
                ShareTransfer.phase(this, "Waiting for computer…")
                // A directed, encrypted BLE offer; only the selected peer gets the capability.
                lastProgress = SystemClock.elapsedRealtime()
                check(ble?.offerShare(peer, token) == true) { "Computer disconnected" }
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
                var last = SystemClock.elapsedRealtime()
                var lastSent = 0L
                ShareTransfer.item(this, index, item.copy(state = "Sending"), bytesPerSecond = 0.0)
                file.inputStream().use { source ->
                    val buf = ByteArray(128 * 1024)
                    while (sent < item.size) {
                        check(!cancelled) { "Cancelled" }
                        val n = source.read(buf, 0, minOf(buf.size.toLong(), item.size - sent).toInt())
                        check(n > 0) { "Source ended early" }
                        output.write(buf, 0, n)
                        sent += n
                        val now = SystemClock.elapsedRealtime()
                        lastProgress = now
                        if (now - last >= 250) {
                            val speed = (sent - lastSent).toDouble() * 1000 / (now - last)
                            ShareTransfer.item(this, index, item.copy(state = "Sending", sent = sent), bytesPerSecond = speed)
                            last = now
                            lastSent = sent
                        }
                    }
                    output.flush()
                }
                ShareTransfer.item(this, index, item.copy(state = "Saving", sent = sent), bytesPerSecond = 0.0)
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
