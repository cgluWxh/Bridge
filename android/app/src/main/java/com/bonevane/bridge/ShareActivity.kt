package com.bonevane.bridge

import android.app.Activity
import android.content.ClipData
import android.content.Intent
import android.net.Uri
import android.os.Bundle
import android.os.Handler
import android.os.Looper
import android.widget.*

/** Share/Open-with target and a re-openable report page. Leaving it never cancels IO. */
class ShareActivity : Activity() {
    private val handler = Handler(Looper.getMainLooper())
    private lateinit var body: LinearLayout
    private lateinit var status: TextView
    private var choosing = false
    private var started = false
    private val tick =
        object : Runnable {
            override fun run() {
                if (!choosing) renderProgress()
                handler.postDelayed(this, 500)
            }
        }

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        body =
            LinearLayout(this).apply {
                orientation = LinearLayout.VERTICAL
                val padding = (20 * resources.displayMetrics.density).toInt()
                setPadding(padding, padding, padding, padding)
            }
        setContentView(
            ScrollView(this).apply {
                addView(body)
                setOnApplyWindowInsetsListener { view, insets ->
                    val bars =
                        insets.getInsets(
                            android.view.WindowInsets.Type
                                .systemBars(),
                        )
                    view.setPadding(bars.left, bars.top, bars.right, bars.bottom)
                    insets
                }
            },
        )
        status =
            TextView(this).apply {
                textSize = 18f
                setTextIsSelectable(true)
            }
        body.addView(status)
        val uris = sharedUris()
        if (isFileIntent() && uris.isEmpty()) {
            choosing = true
            status.text = "No readable file URI was provided. Open or share files from your gallery or file manager."
            body.addView(
                Button(this).apply {
                    text = "Close"
                    setOnClickListener { finish() }
                },
            )
            return
        }
        if (uris.isNotEmpty() && !ShareTransfer.active && savedInstanceState?.getBoolean("started") != true) {
            showPeers(uris)
        } else {
            if (uris.isNotEmpty() &&
                ShareTransfer.active && savedInstanceState == null
            ) {
                Toast
                    .makeText(
                        this,
                        "A transfer is already running. Share these files again after it finishes.",
                        Toast.LENGTH_LONG,
                    ).show()
            }
            showProgressControls()
        }
    }

    private fun isFileIntent(): Boolean = intent.action == Intent.ACTION_SEND ||
        intent.action == Intent.ACTION_SEND_MULTIPLE || intent.action == Intent.ACTION_VIEW

    @Suppress("DEPRECATION")
    private fun sharedUris(): List<Uri> {
        if (!isFileIntent()) return emptyList()
        val streams = when (intent.action) {
            Intent.ACTION_VIEW -> listOfNotNull(intent.data)
            Intent.ACTION_SEND_MULTIPLE -> intent.getParcelableArrayListExtra<Uri>(Intent.EXTRA_STREAM).orEmpty()
            else -> listOfNotNull(intent.getParcelableExtra<Uri>(Intent.EXTRA_STREAM))
        }
        val clip = intent.clipData
        return (streams + (0 until (clip?.itemCount ?: 0)).mapNotNull { clip?.getItemAt(it)?.uri })
            .filter { it.scheme == "content" || it.scheme == "file" }
            .distinct()
    }

    private fun showPeers(uris: List<Uri>) {
        choosing = true
        body.removeViews(1, body.childCount - 1)
        status.text = "Share ${uris.size} file(s) to a computer\nMac receiving is not supported yet."
        if (TunnelService.current == null) runCatching { TunnelService.start(this) }
        val peers =
            TunnelService.current
                ?.ble
                ?.sharePeers()
                .orEmpty()
        if (peers.size == 1 && peers.single().supported && startShare(uris, peers.single())) return
        if (peers.isEmpty()) status.append("\nNo verified computer connected. Open Bridge on your computer and connect Bluetooth first.")
        peers.forEach { peer ->
            body.addView(
                Button(this).apply {
                    text = peer.name + if (peer.supported) "" else " (not supported yet)"
                    isEnabled = peer.supported
                    setOnClickListener { startShare(uris, peer) }
                },
            )
        }
        body.addView(
            Button(this).apply {
                text = "Refresh computers"
                setOnClickListener { showPeers(uris) }
            },
        )
    }

    private fun startShare(
        uris: List<Uri>,
        peer: BleLink.SharePeer,
    ): Boolean {
        if (started || ShareTransfer.active) {
            Toast.makeText(this, "A transfer is already running", Toast.LENGTH_SHORT).show()
            return false
        }
        val service =
            Intent(this, ShareService::class.java)
                .putParcelableArrayListExtra("uris", ArrayList(uris))
                .putExtra("peer", peer.id)
                .putExtra("peerName", peer.name)
                .addFlags(Intent.FLAG_GRANT_READ_URI_PERMISSION)
        // Content URI grants must outlive this activity. Legacy file URIs need
        // no grant: putting them in outgoing ClipData can trigger StrictMode's
        // FileUriExposedException even though the service belongs to this app.
        val granted = uris.filter { it.scheme == "content" }
        if (granted.isNotEmpty()) {
            service.clipData = ClipData.newRawUri("Shared files", granted.first()).also { clip ->
                granted.drop(1).forEach { clip.addItem(ClipData.Item(it)) }
            }
        }
        return try {
            started = true // Also guard repeated taps before the service attaches.
            startForegroundService(service)
            showProgressControls()
            status.text = "Starting transfer…"
            true
        } catch (e: Exception) {
            started = false
            Toast.makeText(this, "Cannot start sharing: ${e.message}", Toast.LENGTH_LONG).show()
            false
        }
    }

    private fun showProgressControls() {
        choosing = false
        body.removeViews(1, body.childCount - 1)
        body.addView(
            Button(this).apply {
                text = "Continue in background"
                setOnClickListener { finish() }
            },
        )
        body.addView(
            Button(this).apply {
                text = "Cancel transfer"
                setOnClickListener { ShareTransfer.cancel() }
            },
        )
    }

    private fun renderProgress() {
        val s = ShareTransfer.snapshot
        status.text =
            buildString {
                append("Sharing to ${s.peer.ifEmpty { "computer" }}\n${s.phase}\n${s.summary}\n")
                if (s.prepared) {
                    append("\nOverall: ${s.overallProgress}\n")
                    append("Speed: ${ShareTransfer.humanBytes(s.currentBytesPerSecond)}/s\n")
                }
                s.items.forEachIndexed { index, item ->
                    append("\n${index + 1}. ${item.name}\n${item.state}")
                    if (item.state != "Queued" && item.state != "Preparing") {
                        append(" · ${ShareTransfer.humanBytes(item.sent.toDouble())} / ${ShareTransfer.humanBytes(item.size.toDouble())}")
                        append(" · ${ShareTransfer.percent(item.fraction)}")
                    }
                    append('\n')
                }
                if (!s.active) append("\nShare again from the source app to retry failed files.")
            }
    }

    override fun onStart() {
        super.onStart()
        handler.post(tick)
    }

    override fun onStop() {
        handler.removeCallbacks(tick)
        super.onStop()
    }

    override fun onSaveInstanceState(outState: Bundle) {
        outState.putBoolean("started", !choosing)
        super.onSaveInstanceState(outState)
    }
}
