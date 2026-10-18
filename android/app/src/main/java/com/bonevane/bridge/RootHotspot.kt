package com.bonevane.bridge

import android.content.Context
import android.os.Handler
import android.os.HandlerThread
import android.util.Base64
import java.lang.reflect.InvocationTargetException
import java.lang.reflect.Proxy
import java.util.concurrent.CountDownLatch
import java.util.concurrent.Executor
import java.util.concurrent.TimeUnit
import java.util.concurrent.atomic.AtomicBoolean
import java.util.concurrent.atomic.AtomicReference

/** Privileged implementation. Responses contain credentials: never log or broadcast them. */
object RootHotspot {
    private val busy = AtomicBoolean(false)
    private val thread by lazy { HandlerThread("Bridge-hotspot").apply { start() } }

    /** OK<TAB>base64(SSID)<TAB>base64(password)<TAB>security, or ERR<TAB>safe error code. */
    fun start(): String {
        if (android.os.Build.VERSION.SDK_INT < 30) return "ERR\tunsupported_android_version"
        if (android.os.Process.myUid() != 0) return "ERR\troot_required"
        if (!busy.compareAndSet(false, true)) return "ERR\tbusy"
        try {
            val done = CountDownLatch(1)
            val result = AtomicReference("ERR\ttimeout_check_system_hotspot")
            Handler(thread.looper).post {
                try {
                    val vm = Class.forName("dalvik.system.VMRuntime")
                    val runtime = vm.getDeclaredMethod("getRuntime").invoke(null)
                    vm
                        .getDeclaredMethod("setHiddenApiExemptions", Array<String>::class.java)
                        .invoke(runtime, arrayOf("L"))
                    val activityThread = Class.forName("android.app.ActivityThread")
                    val systemThread =
                        activityThread.getDeclaredMethod("currentActivityThread").invoke(null)
                            ?: activityThread.getDeclaredMethod("systemMain").invoke(null)
                    val systemContext =
                        activityThread
                            .getDeclaredMethod("getSystemContext")
                            .invoke(systemThread) as Context
                    val context = systemContext.createPackageContext("com.android.shell", 0)
                    val wifi = context.getSystemService("wifi") ?: error("wifi unavailable")

                    fun configuration(): String {
                        val config =
                            wifi.javaClass.getMethod("getSoftApConfiguration").invoke(wifi)
                                ?: error("hotspot configuration unavailable")
                        val ssid =
                            config.javaClass.getMethod("getSsid").invoke(config) as? String
                                ?: error("hotspot name unavailable")
                        val password = config.javaClass.getMethod("getPassphrase").invoke(config) as? String ?: ""
                        val security =
                            when (config.javaClass.getMethod("getSecurityType").invoke(config) as Int) {
                                0 -> "open"
                                1 -> "wpa2"
                                2 -> "wpa2-wpa3"
                                3 -> "wpa3"
                                else -> return "ERR\tunsupported_security"
                            }
                        if (ssid.isEmpty() || ssid.toByteArray(Charsets.UTF_8).size > 32) return "ERR\tinvalid_ssid"
                        if (security != "open" && password.isEmpty()) return "ERR\tpassword_unavailable"

                        fun encode(value: String) = Base64.encodeToString(value.toByteArray(Charsets.UTF_8), Base64.NO_WRAP)
                        return "OK\t${encode(ssid)}\t${encode(if (security == "open") "" else password)}\t$security"
                    }
                    // Validate before changing state, but read again after success in case the ROM changed it.
                    val config = configuration()
                    if (!config.startsWith("OK\t")) {
                        result.set(config)
                        done.countDown()
                        return@post
                    }
                    // WIFI_AP_STATE_ENABLED = 13. Do not restart an existing hotspot.
                    val alreadyEnabled =
                        runCatching {
                            wifi.javaClass.getMethod("getWifiApState").invoke(wifi) as Int == 13
                        }.getOrDefault(false)
                    if (alreadyEnabled) {
                        result.set(config)
                        done.countDown()
                        return@post
                    }
                    val manager = context.getSystemService("tethering") ?: error("tethering unavailable")
                    val callbackClass = Class.forName("android.net.TetheringManager\$StartTetheringCallback")
                    val callback =
                        Proxy.newProxyInstance(callbackClass.classLoader, arrayOf(callbackClass)) { proxy, method, args ->
                            when (method.name) {
                                "onTetheringStarted" -> {
                                    try {
                                        result.set(configuration())
                                    } catch (_: Throwable) {
                                        result.set("ERR\tconfiguration_unavailable")
                                    }
                                    done.countDown()
                                    null
                                }

                                "onTetheringFailed" -> {
                                    val code = args?.firstOrNull() as? Int ?: -1
                                    // Some ROMs reject a duplicate start with 18. Let the
                                    // computer try the saved configuration without toggling Wi-Fi.
                                    if (code == 18) {
                                        result.set(runCatching { configuration() }.getOrDefault("ERR\tconfiguration_unavailable"))
                                    } else {
                                        result.set("ERR\ttethering_$code")
                                    }
                                    done.countDown()
                                    null
                                }

                                "toString" -> {
                                    "BridgeHotspotCallback"
                                }

                                "hashCode" -> {
                                    System.identityHashCode(proxy)
                                }

                                "equals" -> {
                                    proxy === args?.firstOrNull()
                                }

                                else -> {
                                    null
                                }
                            }
                        }
                    // TETHERING_WIFI = 0; no setSoftApConfiguration, no replacement SSID/password.
                    manager.javaClass
                        .getMethod(
                            "startTethering",
                            Int::class.javaPrimitiveType,
                            Executor::class.java,
                            callbackClass,
                        ).invoke(manager, 0, Executor { it.run() }, callback)
                } catch (error: Throwable) {
                    val cause = if (error is InvocationTargetException) error.targetException else error
                    // Exception messages can contain configuration values. Return only a class name.
                    result.set("ERR\tapi_${cause.javaClass.simpleName.replace(Regex("[^A-Za-z0-9_]"), "")}")
                    done.countDown()
                }
            }
            done.await(20, TimeUnit.SECONDS)
            return result.get()
        } finally {
            busy.set(false)
        }
    }
}
