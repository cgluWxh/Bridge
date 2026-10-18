package com.bonevane.bridge

import android.content.Context
import java.net.Inet4Address
import java.net.NetworkInterface
import java.net.Socket
import java.security.KeyPairGenerator
import java.security.KeyStore
import java.security.MessageDigest
import java.util.concurrent.Executors
import java.util.concurrent.Semaphore
import javax.net.ssl.KeyManagerFactory
import javax.net.ssl.SSLContext
import javax.net.ssl.SSLServerSocket
import javax.net.ssl.SSLSocket

/** IPv4 LAN ingress. Only its pinned TLS endpoint is advertised over authenticated BLE.
 * The original loopback ControlProxy remains private and unchanged.
 */
class LanServer(
    private val context: Context,
) {
    @Volatile private var server: SSLServerSocket? = null

    @Volatile private var fingerprint: String? = null

    @Volatile private var stopped = false
    private val clients =
        java.util.concurrent.ConcurrentHashMap
            .newKeySet<Socket>()
    private val slots = Semaphore(24)
    private val workers = Executors.newCachedThreadPool()

    fun start() {
        Thread({
            try {
                val keys = KeyPairGenerator.getInstance("RSA").apply { initialize(2048) }.generateKeyPair()
                val cert = SelfSignedCert.create(keys.public, keys.private, "Bridge LAN")
                val password =
                    java.util.UUID
                        .randomUUID()
                        .toString()
                        .toCharArray()
                val store =
                    KeyStore.getInstance(KeyStore.getDefaultType()).apply {
                        load(null)
                        setKeyEntry("lan", keys.private, password, arrayOf(cert))
                    }
                val managers = KeyManagerFactory.getInstance(KeyManagerFactory.getDefaultAlgorithm()).apply { init(store, password) }
                val tls = SSLContext.getInstance("TLS").apply { init(managers.keyManagers, null, null) }
                val listener =
                    tls.serverSocketFactory.createServerSocket(
                        0,
                        16,
                        java.net.InetAddress.getByName("0.0.0.0"),
                    ) as SSLServerSocket
                listener.enabledProtocols = listener.supportedProtocols.filter { it == "TLSv1.2" || it == "TLSv1.3" }.toTypedArray()
                synchronized(this) {
                    if (stopped) {
                        listener.close()
                        return@Thread
                    }
                    fingerprint =
                        MessageDigest.getInstance("SHA-256").digest(cert.encoded).joinToString("") { "%02x".format(it.toInt() and 255) }
                    server = listener
                }
                TunnelService.current?.ble?.sendStatus()
                while (!listener.isClosed) {
                    val client = listener.accept() as SSLSocket
                    if (!slots.tryAcquire()) {
                        client.close()
                        continue
                    }
                    clients.add(client)
                    workers.execute {
                        try {
                            handle(client)
                        } catch (_: Exception) {
                        } finally {
                            clients.remove(client)
                            runCatching { client.close() }
                            slots.release()
                        }
                    }
                }
            } catch (e: Exception) {
                if (!stopped) TunnelState.log("LAN listener stopped: ${e.message}")
            } finally {
                synchronized(this) {
                    runCatching { server?.close() }
                    server = null
                    fingerprint = null
                }
            }
        }, "lan-listener").start()
    }

    /** No secrets in the advertisement; certificate identity is trusted only via encrypted BLE. */
    fun status(): String {
        val listener = server ?: return ""
        val pin = fingerprint ?: return ""
        val addresses =
            runCatching {
                java.util.Collections
                    .list(NetworkInterface.getNetworkInterfaces())
                    .filter { it.isUp && !it.isLoopback }
                    .flatMap { java.util.Collections.list(it.inetAddresses) }
                    .filterIsInstance<Inet4Address>()
                    .filter { !it.isLoopbackAddress && !it.isLinkLocalAddress && !it.isAnyLocalAddress && !it.isMulticastAddress }
                    .mapNotNull { it.hostAddress }
                    .distinct()
                    .take(8)
            }.getOrDefault(emptyList())
        if (addresses.isEmpty()) return ""
        return " lan_port=${listener.localPort} lan_fp=$pin lan_ips=${addresses.joinToString(",")}"
    }

    private fun handle(client: SSLSocket) {
        client.soTimeout = 3000
        client.tcpNoDelay = true
        client.startHandshake()
        // Authenticate BEFORE opening the private proxy. Do not expose an unauthenticated
        // LAN-to-loopback relay. AUTH is forwarded only after TLS pairing validation.
        val input = client.inputStream
        val line = StringBuilder()
        while (true) {
            val b = input.read()
            check(b >= 0 && line.length < 200) { "Missing AUTH" }
            if (b == 10) break
            line.append(b.toChar())
        }
        val auth = line.toString()
        check(auth.startsWith("AUTH ") && Pairing.matches(context, auth.substring(5))) { "Unauthorized" }
        val local = Socket()
        clients.add(local)
        try {
            local.connect(java.net.InetSocketAddress("127.0.0.1", ControlProxy.PORT), 1000)
            local.tcpNoDelay = true
            local.getOutputStream().write((auth + "\n").toByteArray())
            client.soTimeout = 0
            val upstream =
                workers.submit {
                    try {
                        input.copyTo(local.getOutputStream(), 64 * 1024)
                    } catch (
                        _: Exception,
                    ) {
                    } finally {
                        runCatching { local.close() }
                        runCatching { client.close() }
                    }
                }
            try {
                local.getInputStream().copyTo(client.outputStream, 64 * 1024)
            } finally {
                runCatching { client.close() }
                runCatching { local.close() }
                upstream.cancel(true)
            }
        } finally {
            clients.remove(local)
            runCatching { local.close() }
        }
    }

    @Synchronized fun stop() {
        stopped = true
        runCatching { server?.close() }
        server = null
        fingerprint = null
        clients.forEach { runCatching { it.close() } }
        workers.shutdownNow()
    }
}
