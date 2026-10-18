package com.bonevane.bridge

import android.content.ClipData
import android.content.ClipboardManager
import android.content.ContentProvider
import android.content.ContentValues
import android.content.Context
import android.database.Cursor
import android.database.MatrixCursor
import android.graphics.BitmapFactory
import android.net.Uri
import android.os.ParcelFileDescriptor
import android.provider.OpenableColumns
import java.io.File
import java.net.Socket
import java.util.UUID

/** App-owned, grantable image URIs. No activity launch and no broad storage access. */
object ImageClipboard {
    const val AUTHORITY = "com.bonevane.bridge.images"
    const val PREFIX = "content://$AUTHORITY/"
    private const val MAX_BYTES = 32L * 1024 * 1024

    fun directory(context: Context) = File(context.cacheDir, "clipboard-images").also { it.mkdirs() }

    private data class Grant(val size: Long, val expires: Long)
    private val grants = java.util.concurrent.ConcurrentHashMap<String, Grant>()

    /** Issued only by the verified BLE screen-on admission, before LAN probing. */
    fun grant(token: String, size: Long): Boolean {
        val now = android.os.SystemClock.elapsedRealtime()
        grants.entries.removeIf { it.value.expires <= now }
        if (!token.matches(Regex("[a-f0-9]{32}")) || size !in 1..MAX_BYTES || grants.size >= 64) return false
        return grants.putIfAbsent(token, Grant(size, now + 60_000)) == null
    }

    fun revokeGrant(token: String) { grants.remove(token) }

    /** Single use for either LAN admission or BLE begin; no screen recheck. */
    fun consumeGrant(token: String, size: Long): Boolean {
        val grant = grants.remove(token) ?: return false
        return grant.size == size && grant.expires > android.os.SystemClock.elapsedRealtime()
    }

    /** IMGC <PNG byte length> -> READY -> PNG -> OK. Runs after pairing AUTH. */
    @Synchronized
    fun receive(
        context: Context,
        socket: Socket,
        header: String,
        requireMirror: Boolean = true,
    ) {
        socket.soTimeout = 60_000
        val out = socket.getOutputStream()

        fun reply(line: String) {
            out.write("$line\n".toByteArray())
            out.flush()
        }
        var file: File? = null
        try {
            if (requireMirror) check(TunnelState.mirroring) { "Mirroring is not active" }
            val fields = header.trim().split(' ', limit = 2)
            val size = fields[0].toLongOrNull() ?: error("Invalid image length")
            require(size in 1..MAX_BYTES) { "Image exceeds size limit" }
            if (!requireMirror) {
                check(fields.size == 2 && consumeGrant(fields[1], size)) { "Missing or expired BLE image admission" }
            }
            val dir = directory(context)
            // Retain previous images for 24 h: a chat app may read its URI later.
            dir
                .listFiles()
                ?.filter { System.currentTimeMillis() - it.lastModified() > 24 * 60 * 60 * 1000L }
                ?.forEach { it.delete() }
            require(dir.listFiles().orEmpty().sumOf { it.length() } + size <= 256L * 1024 * 1024) {
                "Clipboard image cache is full; try again later"
            }
            val target = File(dir, "${UUID.randomUUID()}.png")
            file = target
            check(target.createNewFile()) { "Could not create image" }
            reply("READY")
            target.outputStream().use { output ->
                val input = socket.getInputStream()
                val buffer = ByteArray(128 * 1024)
                var left = size
                while (left > 0) {
                    if (requireMirror) check(TunnelState.mirroring) { "Mirroring ended" }
                    val n = input.read(buffer, 0, minOf(left, buffer.size.toLong()).toInt())
                    check(n > 0) { "Incomplete image" }
                    output.write(buffer, 0, n)
                    left -= n
                }
            }
            if (requireMirror) check(TunnelState.mirroring) { "Mirroring ended" }
            publish(context, target)
            file = null // Published: retain for delayed reads, even if ACK is lost.
            reply("OK")
        } catch (e: Exception) {
            runCatching { reply("ERR ${e.message.orEmpty().replace('\n', ' ').take(160)}") }
        } finally {
            file?.delete()
        }
    }

    /** Shared by tunnel and BLE; publishing does not require a mirror or a lit screen. */
    fun publish(
        context: Context,
        target: File,
    ) {
        val bounds = BitmapFactory.Options().apply { inJustDecodeBounds = true }
        BitmapFactory.decodeFile(target.path, bounds)
        require(
            bounds.outMimeType == "image/png" && bounds.outWidth > 0 && bounds.outHeight > 0 &&
                bounds.outWidth.toLong() * bounds.outHeight <= 40_000_000,
        ) { "Invalid or oversized PNG" }
        val uri = Uri.parse("$PREFIX${target.name}")
        // Android grants the focused clipboard consumer access through the
        // provider's grantUriPermissions contract. Do not grant every app.
        context
            .getSystemService(
                ClipboardManager::class.java,
            ).setPrimaryClip(ClipData.newUri(context.contentResolver, "PC screenshot", uri))
    }
}

/** Read-only provider; only the clipboard service's URI grants allow other apps in. */
class ImageClipboardProvider : ContentProvider() {
    override fun onCreate() = true

    private fun resolve(uri: Uri): File {
        require(uri.authority == ImageClipboard.AUTHORITY && uri.pathSegments.size == 1) { "Invalid image URI" }
        val name = uri.pathSegments.single()
        require(Regex("[a-f0-9-]{36}\\.png").matches(name)) { "Invalid image name" }
        return File(ImageClipboard.directory(requireNotNull(context)), name)
    }

    override fun getType(uri: Uri): String {
        resolve(uri)
        return "image/png"
    }

    override fun openFile(
        uri: Uri,
        mode: String,
    ): ParcelFileDescriptor {
        require(mode == "r") { "Read only" }
        return ParcelFileDescriptor.open(resolve(uri), ParcelFileDescriptor.MODE_READ_ONLY)
    }

    override fun query(
        uri: Uri,
        projection: Array<out String>?,
        selection: String?,
        selectionArgs: Array<out String>?,
        sortOrder: String?,
    ): Cursor {
        val file = resolve(uri)
        val columns = projection ?: arrayOf(OpenableColumns.DISPLAY_NAME, OpenableColumns.SIZE)
        return MatrixCursor(columns).apply {
            addRow(
                columns
                    .map {
                        when (it) {
                            OpenableColumns.DISPLAY_NAME -> "PC screenshot.png"
                            OpenableColumns.SIZE -> file.length()
                            else -> null
                        }
                    }.toTypedArray(),
            )
        }
    }

    override fun insert(
        uri: Uri,
        values: ContentValues?,
    ): Uri? = throw UnsupportedOperationException("Read only")

    override fun update(
        uri: Uri,
        values: ContentValues?,
        selection: String?,
        selectionArgs: Array<out String>?,
    ): Int = throw UnsupportedOperationException("Read only")

    override fun delete(
        uri: Uri,
        selection: String?,
        selectionArgs: Array<out String>?,
    ): Int = throw UnsupportedOperationException("Read only")
}
