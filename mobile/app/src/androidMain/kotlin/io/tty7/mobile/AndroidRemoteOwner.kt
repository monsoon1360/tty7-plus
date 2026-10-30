package io.tty7.mobile

import android.app.Service
import android.content.Context
import android.content.Intent
import android.content.pm.ServiceInfo
import android.net.ConnectivityManager
import android.net.Network
import android.net.NetworkCapabilities
import android.os.Build
import android.os.Handler
import android.os.IBinder
import android.os.Looper
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.SupervisorJob
import kotlinx.coroutines.cancel
import kotlinx.coroutines.launch
import kotlinx.coroutines.flow.collect

/** One authenticated connection per process; never retains an Activity or a Compose scope. */
internal class AndroidRemoteOwner private constructor(context: Context) {
    private val app = context.applicationContext
    private val scope = CoroutineScope(SupervisorJob() + Dispatchers.Main.immediate)
    val platform = AndroidPlatform(app)
    val controller = RemoteController(platform, scope)
    private val preferences = app.getSharedPreferences("agent_monitor", Context.MODE_PRIVATE)
    private val handler = Handler(Looper.getMainLooper())
    private val connectivity = app.getSystemService(ConnectivityManager::class.java)
    private var networkCallback: ConnectivityManager.NetworkCallback? = null
    private var defaultNetwork: Network? = null
    private var networkAvailable = true
    private var activities = 0
    private var foreground = false
    private var serviceRunning = false
    private var serviceRequested = false
    private var wasActive = false

    init {
        if (!preferences.getBoolean("automatic_connection", true)) controller.disconnect()
        scope.launch {
            controller.state.collect { state ->
                // Transport errors briefly publish Disconnected before retrying; they
                // must not stop the service that owns that retry in the background.
                val active = state.connectionRequested
                if (active != wasActive) {
                    if (active || foreground || serviceRunning) preferences.edit().putBoolean("automatic_connection", active).apply()
                    wasActive = active
                }
                if (active && foreground && !serviceRunning && !serviceRequested) {
                    serviceRequested = true
                    try {
                        app.startForegroundService(Intent(app, AgentMonitorService::class.java))
                        // Bridge Activity.onStop until Service.onCreate can assume ownership.
                        controller.setBackgroundMonitoring(true)
                    }
                    catch (_: IllegalStateException) { serviceRequested = false }
                    catch (_: SecurityException) { serviceRequested = false }
                } else if (!active && (serviceRunning || serviceRequested)) {
                    app.stopService(Intent(app, AgentMonitorService::class.java))
                    serviceRequested = false
                    controller.setBackgroundMonitoring(false)
                }
            }
        }
    }

    fun attachActivity() { activities++ }

    fun setForeground(value: Boolean) {
        foreground = value
        if (value) monitorNetwork()
        controller.setForeground(value)
        if (!value && !serviceRunning && !serviceRequested) stopNetwork()
    }

    fun detachActivity(changingConfiguration: Boolean) {
        activities--
        if (!changingConfiguration) releaseIfUnused()
    }

    fun serviceStarted() {
        serviceRequested = false
        serviceRunning = true
        monitorNetwork()
        controller.setBackgroundMonitoring(true)
    }

    fun serviceStopped() {
        serviceRunning = false
        serviceRequested = false
        controller.setBackgroundMonitoring(false)
        if (!foreground) stopNetwork()
        releaseIfUnused()
    }

    private fun releaseIfUnused() {
        if (activities != 0 || serviceRunning || serviceRequested) return
        stopNetwork()
        controller.disconnect()
        scope.cancel()
        if (instance === this) instance = null
    }

    private fun updateNetwork(network: Network?, available: Boolean) {
        if (defaultNetwork == network && networkAvailable == available) return
        defaultNetwork = network
        networkAvailable = available
        controller.networkChanged(available)
    }

    private fun monitorNetwork() {
        if (networkCallback != null) return
        val active = connectivity.activeNetwork
        updateNetwork(active, connectivity.getNetworkCapabilities(active)?.hasCapability(NetworkCapabilities.NET_CAPABILITY_INTERNET) == true)
        val callback = object : ConnectivityManager.NetworkCallback() {
            override fun onAvailable(network: Network) {
                handler.post {
                    if (networkCallback === this) updateNetwork(network, connectivity.getNetworkCapabilities(network)?.hasCapability(NetworkCapabilities.NET_CAPABILITY_INTERNET) == true)
                }
            }
            override fun onCapabilitiesChanged(network: Network, capabilities: NetworkCapabilities) {
                handler.post {
                    if (networkCallback === this && defaultNetwork == network) updateNetwork(network, capabilities.hasCapability(NetworkCapabilities.NET_CAPABILITY_INTERNET))
                }
            }
            override fun onLost(network: Network) {
                handler.post { if (networkCallback === this && defaultNetwork == network) updateNetwork(null, false) }
            }
        }
        networkCallback = callback
        connectivity.registerDefaultNetworkCallback(callback)
    }

    private fun stopNetwork() {
        networkCallback?.let { connectivity.unregisterNetworkCallback(it) }
        networkCallback = null
    }

    companion object {
        private var instance: AndroidRemoteOwner? = null
        fun get(context: Context): AndroidRemoteOwner = instance ?: AndroidRemoteOwner(context).also { instance = it }
    }
}

class AgentMonitorService : Service() {
    private lateinit var owner: AndroidRemoteOwner

    override fun onCreate() {
        super.onCreate()
        val notification = AgentNotifications.monitor(this)
        if (Build.VERSION.SDK_INT >= 29) {
            startForeground(AgentNotifications.MONITOR_ID, notification, ServiceInfo.FOREGROUND_SERVICE_TYPE_CONNECTED_DEVICE)
        } else startForeground(AgentNotifications.MONITOR_ID, notification)
        owner = AndroidRemoteOwner.get(applicationContext)
        owner.serviceStarted()
    }

    // No sticky resurrection: Android's Stop control and explicit disconnect remain authoritative.
    override fun onStartCommand(intent: Intent?, flags: Int, startId: Int): Int = START_NOT_STICKY
    override fun onBind(intent: Intent?): IBinder? = null
    override fun onDestroy() {
        owner.serviceStopped()
        stopForeground(STOP_FOREGROUND_REMOVE)
        super.onDestroy()
    }
}
