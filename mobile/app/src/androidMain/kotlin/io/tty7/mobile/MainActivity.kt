package io.tty7.mobile

import android.Manifest
import android.content.Intent
import android.content.pm.PackageManager
import android.os.Build
import android.os.Bundle
import androidx.activity.ComponentActivity
import androidx.activity.compose.setContent
import androidx.activity.enableEdgeToEdge
import androidx.activity.result.contract.ActivityResultContracts
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
    private lateinit var owner: AndroidRemoteOwner
    private var pendingPairingLink: String? = null
    private val notificationPermission = registerForActivityResult(ActivityResultContracts.RequestPermission()) {
        // Denial disables task alerts, not the permitted foreground connection service.
    }
    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        enableEdgeToEdge()
        owner = AndroidRemoteOwner.get(applicationContext)
        owner.attachActivity()
        AgentNotifications.channels(this)
        if (Build.VERSION.SDK_INT >= 33 && checkSelfPermission(Manifest.permission.POST_NOTIFICATIONS) != PackageManager.PERMISSION_GRANTED) {
            val preferences = getSharedPreferences("agent_monitor", MODE_PRIVATE)
            if (!preferences.getBoolean("notification_permission_requested", false)) {
                preferences.edit().putBoolean("notification_permission_requested", true).apply()
                notificationPermission.launch(Manifest.permission.POST_NOTIFICATIONS)
            }
        }
        // Do not automatically replay an old pairing deep link after rotation.
        if (savedInstanceState == null) receivePairingIntent(intent)
        setContent {
            Box(Modifier.fillMaxSize().semantics { testTagsAsResourceId = BuildConfig.DEBUG }) {
                MobileApp(
                    owner.platform, linkFlow, owner.controller,
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
    // Network monitoring lives in AndroidRemoteOwner, including while this Activity is stopped.
    override fun onStart() {
        super.onStart()
        owner.setForeground(true)
        pendingPairingLink?.let { pairingLinks.trySend(it) }
        pendingPairingLink = null
    }
    override fun onStop() {
        owner.setForeground(false)
        super.onStop()
    }
    override fun onDestroy() {
        owner.detachActivity(isChangingConfigurations)
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
