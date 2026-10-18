package com.bonevane.bridge

import android.app.Notification
import android.app.NotificationChannel
import android.app.NotificationManager
import android.app.PendingIntent
import android.content.ClipData
import android.content.ContentValues
import android.content.Context
import android.content.Intent
import android.net.Uri
import android.os.Environment
import android.provider.MediaStore
import android.webkit.MimeTypeMap
import java.net.Socket

/** App-owned downloads provide grantable content URIs for notification actions. */
object ReceivedFiles {
    private const val CHANNEL = "received_files"
    private const val MAX_SIZE = 4L * 1024 * 1024 * 1024

    fun receive(
        ctx: Context,
        socket: Socket,
    ) {
        // Decode the complete UTF-8 header without consuming binary payload.
        val header = java.io.ByteArrayOutputStream()
        val input = socket.getInputStream()
        while (true) {
            val byte = input.read()
            if (byte < 0 || header.size() >= 8192) {
                reply(socket, "ERR invalid upload header")
                return
            }
            if (byte == '\n'.code) break
            header.write(byte)
        }
        val args = header.toString("UTF-8").trimStart()
        val size = args.substringBefore(' ').trim().toLongOrNull() ?: 0
        val name =
            args
                .substringAfter(' ', "")
                .trim()
                .substringAfterLast('/')
                .substringAfterLast('\\')
                .takeIf { it.isNotBlank() && it != "." && it != ".." }
                ?: "bridge-file"
        if (size <= 0 || size > MAX_SIZE) {
            reply(socket, "ERR size")
            return
        }
        val mime =
            MimeTypeMap.getSingleton().getMimeTypeFromExtension(
                name.substringAfterLast('.', "").lowercase(java.util.Locale.ROOT),
            ) ?: "application/octet-stream"
        val resolver = ctx.contentResolver
        var pending: Uri? = null
        val saved =
            runCatching {
                val values =
                    ContentValues().apply {
                        put(MediaStore.MediaColumns.DISPLAY_NAME, name)
                        put(MediaStore.MediaColumns.MIME_TYPE, mime)
                        put(MediaStore.MediaColumns.RELATIVE_PATH, Environment.DIRECTORY_DOWNLOADS)
                        put(MediaStore.MediaColumns.IS_PENDING, 1)
                    }
                val uri =
                    resolver.insert(MediaStore.Downloads.EXTERNAL_CONTENT_URI, values)
                        ?: error("Could not create download")
                pending = uri
                resolver.openOutputStream(uri, "w")?.use { output ->
                    val buffer = ByteArray(64 * 1024)
                    var left = size
                    while (left > 0) {
                        val count = input.read(buffer, 0, minOf(left, buffer.size.toLong()).toInt())
                        if (count < 0) error("Upload interrupted")
                        output.write(buffer, 0, count)
                        left -= count
                    }
                } ?: error("Could not write download")
                check(
                    resolver.update(
                        uri,
                        ContentValues().apply {
                            put(MediaStore.MediaColumns.IS_PENDING, 0)
                        },
                        null,
                        null,
                    ) == 1,
                ) { "Could not publish download" }
                uri
            }
        val uri =
            saved.getOrElse { error ->
                pending?.let { runCatching { resolver.delete(it, null, null) } }
                reply(socket, "ERR ${error.message.orEmpty().replace('\n', ' ')}")
                return
            }
        // Notification permission denial must not turn a saved file into a failed upload.
        runCatching { notifyReceived(ctx, uri, name, mime) }
            .onFailure { TunnelState.log("File saved, but notification unavailable: ${it.message}") }
        reply(socket, "OK saved to Download/$name")
    }

    private fun reply(
        socket: Socket,
        text: String,
    ) {
        socket.getOutputStream().apply {
            write("$text\n".toByteArray())
            flush()
        }
    }

    private fun notifyReceived(
        ctx: Context,
        uri: Uri,
        name: String,
        mime: String,
    ) {
        val manager = ctx.getSystemService(NotificationManager::class.java)
        manager.createNotificationChannel(
            NotificationChannel(
                CHANNEL,
                "Received files",
                NotificationManager.IMPORTANCE_DEFAULT,
            ),
        )
        val open =
            Intent(Intent.ACTION_VIEW).apply {
                setDataAndType(uri, mime)
                clipData = ClipData.newRawUri(name, uri)
                addFlags(Intent.FLAG_GRANT_READ_URI_PERMISSION)
            }
        val send =
            Intent(Intent.ACTION_SEND).apply {
                type = mime
                putExtra(Intent.EXTRA_STREAM, uri)
                clipData = ClipData.newRawUri(name, uri)
                addFlags(Intent.FLAG_GRANT_READ_URI_PERMISSION)
            }
        // Distinct identifiers prevent one file's buttons from opening a later file.
        val openChooser =
            Intent.createChooser(open, "Open with").apply {
                identifier = "open:$uri"
                addFlags(Intent.FLAG_ACTIVITY_NEW_TASK or Intent.FLAG_GRANT_READ_URI_PERMISSION)
            }
        val shareChooser =
            Intent.createChooser(send, "Share file").apply {
                identifier = "share:$uri"
                addFlags(Intent.FLAG_ACTIVITY_NEW_TASK or Intent.FLAG_GRANT_READ_URI_PERMISSION)
            }
        val flags = PendingIntent.FLAG_UPDATE_CURRENT or PendingIntent.FLAG_IMMUTABLE
        val openAction = PendingIntent.getActivity(ctx, 0, openChooser, flags)
        val shareAction = PendingIntent.getActivity(ctx, 0, shareChooser, flags)
        manager.notify(
            uri.toString(),
            0,
            Notification
                .Builder(ctx, CHANNEL)
                .setSmallIcon(R.drawable.ic_stat_bridge)
                .setContentTitle("File received")
                .setContentText(name)
                .setContentIntent(openAction)
                .setAutoCancel(true)
                .addAction(Notification.Action.Builder(null, "Open", openAction).build())
                .addAction(Notification.Action.Builder(null, "Share", shareAction).build())
                .build(),
        )
    }
}
