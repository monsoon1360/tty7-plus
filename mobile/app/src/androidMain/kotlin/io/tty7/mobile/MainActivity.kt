package io.tty7.mobile

import android.content.Intent
import android.net.ConnectivityManager
import android.net.Network
import android.net.NetworkCapabilities
import android.os.Bundle
import androidx.activity.ComponentActivity
import androidx.activity.compose.setContent
import androidx.activity.enableEdgeToEdge
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.ui.Modifier
import androidx.compose.ui.semantics.semantics
import androidx.compose.ui.semantics.testTagsAsResourceId
import kotlinx.coroutines.channels.Channel
import kotlinx.coroutines.flow.receiveAsFlow

@OptIn(androidx.compose.ui.ExperimentalComposeUiApi::class)
class MainActivity : ComponentActivity() {
    private val pairingLinks = Channel<String>(Channel.BUFFERED)
    private val linkFlow = pairingLinks.receiveAsFlow()
    private var foregroundRemote: ((Boolean) -> Unit)? = null
    private var networkRemote: ((Boolean) -> Unit)? = null
    private val connectivity by lazy { getSystemService(ConnectivityManager::class.java) }
    private var defaultNetwork: Network? = null
    private var networkAvailable = false
    private var networkCallback: ConnectivityManager.NetworkCallback? = null
    private var pendingPairingLink: String? = null
    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        enableEdgeToEdge()
        val platform = AndroidPlatform(this)
        // Do not automatically replay an old pairing deep link after rotation.
        if (savedInstanceState == null) receivePairingIntent(intent)
        setContent {
            Box(Modifier.fillMaxSize().semantics { testTagsAsResourceId = BuildConfig.DEBUG }) {
                MobileApp(
                    platform, linkFlow,
                    registerLifecycle = {
                        foregroundRemote = it
                        it?.invoke(lifecycle.currentState.isAtLeast(androidx.lifecycle.Lifecycle.State.STARTED))
                    },
                    registerNetwork = {
                        networkRemote = it
                        it?.invoke(networkAvailable)
                    },
                )
            }
        }
    }
    override fun onNewIntent(intent: Intent) {
        super.onNewIntent(intent)
        setIntent(intent)
        receivePairingIntent(intent)
    }
    private fun receivePairingIntent(intent: Intent?) {
        if (intent?.action == Intent.ACTION_VIEW) intent.data?.takeIf { it.scheme == "tty7" && it.host == "pair" }?.toString()?.let {
            if (lifecycle.currentState.isAtLeast(androidx.lifecycle.Lifecycle.State.STARTED)) pairingLinks.trySend(it)
            else pendingPairingLink = it
        }
    }
    private fun updateNetwork(network: Network?, available: Boolean) {
        if (defaultNetwork == network && networkAvailable == available) return
        defaultNetwork = network
        networkAvailable = available
        networkRemote?.invoke(available)
    }
    private fun monitorNetwork() {
        val active = connectivity.activeNetwork
        updateNetwork(active, connectivity.getNetworkCapabilities(active)?.hasCapability(NetworkCapabilities.NET_CAPABILITY_INTERNET) == true)
        val callback = object : ConnectivityManager.NetworkCallback() {
            override fun onAvailable(network: Network) {
                runOnUiThread {
                    if (networkCallback === this) {
                        updateNetwork(network, connectivity.getNetworkCapabilities(network)?.hasCapability(NetworkCapabilities.NET_CAPABILITY_INTERNET) == true)
                    }
                }
            }
            override fun onCapabilitiesChanged(network: Network, capabilities: NetworkCapabilities) {
                runOnUiThread {
                    if (networkCallback === this && defaultNetwork == network) {
                        updateNetwork(network, capabilities.hasCapability(NetworkCapabilities.NET_CAPABILITY_INTERNET))
                    }
                }
            }
            override fun onLost(network: Network) {
                runOnUiThread {
                    if (networkCallback === this && defaultNetwork == network) updateNetwork(null, false)
                }
            }
        }
        networkCallback = callback
        connectivity.registerDefaultNetworkCallback(callback)
    }
    override fun onStart() {
        super.onStart()
        monitorNetwork()
        foregroundRemote?.invoke(true)
        pendingPairingLink?.let { pairingLinks.trySend(it) }
        pendingPairingLink = null
    }
    override fun onStop() {
        foregroundRemote?.invoke(false)
        networkCallback?.let { connectivity.unregisterNetworkCallback(it) }
        networkCallback = null
        super.onStop()
    }
    override fun onDestroy() {
        foregroundRemote?.invoke(false)
        pairingLinks.close()
        super.onDestroy()
    }
}

@androidx.compose.runtime.Composable
actual fun NavigationBackHandler(enabled: Boolean, onBack: () -> Unit) {
    androidx.activity.compose.BackHandler(enabled, onBack)
}

@androidx.compose.runtime.Composable
actual fun ApplySystemAppearance(dark: Boolean) {
    val activity = androidx.compose.ui.platform.LocalContext.current as? android.app.Activity
    androidx.compose.runtime.LaunchedEffect(activity, dark) {
        val style = if (dark) androidx.activity.SystemBarStyle.dark(android.graphics.Color.TRANSPARENT)
            else androidx.activity.SystemBarStyle.light(android.graphics.Color.TRANSPARENT, android.graphics.Color.TRANSPARENT)
        activity?.let { (it as? ComponentActivity)?.enableEdgeToEdge(statusBarStyle = style, navigationBarStyle = style) }
    }
}
