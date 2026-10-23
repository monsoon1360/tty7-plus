package io.tty7.mobile

import android.annotation.SuppressLint
import android.os.Handler
import android.os.Looper
import android.view.inputmethod.InputMethodManager
import android.webkit.*
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.material3.Text
import androidx.compose.runtime.*
import androidx.compose.ui.Modifier
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.platform.LocalFocusManager
import androidx.compose.ui.viewinterop.AndroidView
import androidx.webkit.WebViewAssetLoader
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.withContext
import kotlinx.coroutines.channels.Channel
import kotlinx.coroutines.Job
import kotlinx.coroutines.launch
import kotlinx.coroutines.flow.first
import kotlinx.coroutines.flow.Flow
import kotlinx.coroutines.withTimeout
import kotlinx.coroutines.selects.select
import kotlinx.serialization.json.JsonPrimitive
import java.io.ByteArrayInputStream

private const val TERMINAL_URL = "https://appassets.androidplatform.net/assets/terminal/index.html"
private sealed interface TerminalRenderEvent {
    val generation: Long
    data class Begun(override val generation: Long) : TerminalRenderEvent
    data class Rendered(override val generation: Long, val sequence: Long) : TerminalRenderEvent
    data class Failed(override val generation: Long, val message: String) : TerminalRenderEvent
}
private class TerminalBridge(
    private val onInput: (Long, String) -> Unit,
    private val onResize: (Int, Int) -> Unit,
    private val onFocusInput: (Long) -> Unit,
    private val onReady: () -> Unit,
    private val onHistory: (Long, String?) -> Unit,
    private val events: Channel<TerminalRenderEvent>,
) {
    private val main = Handler(Looper.getMainLooper())
    @Volatile private var disposed = false
    private fun post(block: () -> Unit) { if (!disposed) main.post { if (!disposed) block() } }
    @JavascriptInterface fun input(generation: Long, text: String) { if (text.length <= 65_536) post { onInput(generation, text) } }
    @JavascriptInterface fun resize(cols: Int, rows: Int) { if (cols in 20..1000 && rows in 5..1000) post { onResize(cols, rows) } }
    @JavascriptInterface fun focusInput(generation: Long) = post { onFocusInput(generation) }
    @JavascriptInterface fun ready() = post(onReady)
    @JavascriptInterface fun history(generation: Long, cursor: String) { if (cursor.length <= 1024) post { onHistory(generation, cursor.takeIf { it.isNotEmpty() }) } }
    @JavascriptInterface fun begun(generation: Long) { if (!disposed) events.trySend(TerminalRenderEvent.Begun(generation)) }
    @JavascriptInterface fun rendered(generation: Long, sequence: Long) { if (!disposed) events.trySend(TerminalRenderEvent.Rendered(generation, sequence)) }
    @JavascriptInterface fun failed(generation: Long, message: String) { if (!disposed) events.trySend(TerminalRenderEvent.Failed(generation, message.take(1024))) }
    fun dispose() { disposed = true; main.removeCallbacksAndMessages(null) }
}
private fun WebView.addTerminalBridge(bridge: TerminalBridge) {
    addJavascriptInterface(bridge, "Tty7")
}

@Composable
actual fun TerminalSurface(feed: TerminalFeed?, synced: Boolean, onInput: (String) -> Unit, onResize: (Int, Int) -> Unit, onFailure: (String) -> Unit,
    shortcuts: Flow<String>, agentKind: String?) {
    var rendererVersion by remember { mutableIntStateOf(0) }
    key(rendererVersion) {
        AndroidTerminalSurface(feed, synced, onInput, onResize, onFailure, shortcuts, agentKind) { rendererVersion++ }
    }
}
@SuppressLint("SetJavaScriptEnabled")
@Composable
private fun AndroidTerminalSurface(feed: TerminalFeed?, synced: Boolean, onInput: (String) -> Unit,
    onResize: (Int, Int) -> Unit, onFailure: (String) -> Unit, shortcuts: Flow<String>, agentKind: String?, onRendererGone: () -> Unit) {
    val context = LocalContext.current
    val dark = LocalMobileDarkTheme.current
    val focusManager = LocalFocusManager.current
    val scope = rememberCoroutineScope()
    val currentFeed by rememberUpdatedState(feed)
    val currentInput by rememberUpdatedState(onInput)
    val currentResize by rememberUpdatedState(onResize)
    val currentFailure by rememberUpdatedState(onFailure)
    val events = remember { Channel<TerminalRenderEvent>(64) }
    var error by remember(feed) { mutableStateOf<String?>(null) }
    var pageReady by remember { mutableStateOf(false) }
    var generation by remember { mutableLongStateOf(0) }
    var focusRequest by remember { mutableIntStateOf(0) }
    var webRef by remember { mutableStateOf<WebView?>(null) }
    var historyBusy by remember { mutableStateOf(false) }
    var historyJob by remember { mutableStateOf<Job?>(null) }
    val bridge: TerminalBridge = remember {
        TerminalBridge(
            { gen, text -> if (gen == generation && currentFeed?.synced == true) currentInput(text) },
            { cols, rows -> currentResize(cols, rows) },
            { gen -> if (gen == generation && currentFeed?.synced == true) focusRequest++ },
            { pageReady = true },
            { gen, cursor ->
                val target = currentFeed
                if (gen == generation && target?.synced == true && !historyBusy) {
                    historyBusy = true
                    historyJob = scope.launch {
                        try {
                            val page = target.history(cursor)
                            if (gen == generation && currentFeed === target && target.active) {
                                webRef?.evaluateJavascript("window.tty7History($gen,${page.toJson()})", null)
                            }
                        } catch (e: kotlinx.coroutines.CancellationException) { throw e }
                        catch (e: Exception) {
                            if (gen == generation && currentFeed === target) {
                                webRef?.evaluateJavascript("window.tty7HistoryError($gen,${JsonPrimitive(e.message ?: "历史加载失败")})", null)
                            }
                        } finally { if (gen == generation) historyBusy = false }
                    }
                }
            }, events,
        )
    }
    val web = remember {
        val assets = WebViewAssetLoader.Builder().addPathHandler("/assets/", WebViewAssetLoader.AssetsPathHandler(context)).build()
        WebView(context).apply {
            contentDescription = "远程终端。点击光标直接输入，上下滑动查看历史。"
            settings.javaScriptEnabled = true
            settings.allowFileAccess = false
            settings.allowContentAccess = false
            settings.domStorageEnabled = false
            settings.databaseEnabled = false
            settings.javaScriptCanOpenWindowsAutomatically = false
            settings.setSupportMultipleWindows(false)
            settings.mixedContentMode = WebSettings.MIXED_CONTENT_NEVER_ALLOW
            settings.cacheMode = WebSettings.LOAD_NO_CACHE
            settings.mediaPlaybackRequiresUserGesture = true
            settings.safeBrowsingEnabled = true
            isHorizontalScrollBarEnabled = true
            isFocusableInTouchMode = true
            setBackgroundColor(if (dark) android.graphics.Color.rgb(30, 30, 32) else android.graphics.Color.rgb(252, 252, 251))
            webViewClient = object : WebViewClient() {
                override fun shouldOverrideUrlLoading(view: WebView, request: WebResourceRequest): Boolean = true
                override fun shouldInterceptRequest(view: WebView, request: WebResourceRequest): WebResourceResponse =
                    assets.shouldInterceptRequest(request.url) ?: WebResourceResponse("text/plain", "UTF-8", 403, "Forbidden", emptyMap(), ByteArrayInputStream(ByteArray(0)))
                override fun onReceivedError(view: WebView, request: WebResourceRequest, error: WebResourceError) {
                    if (request.url.toString().startsWith(TERMINAL_URL.substringBeforeLast('/'))) events.trySend(TerminalRenderEvent.Failed(generation, "终端页面加载失败：${error.description}"))
                }
                override fun onReceivedHttpError(view: WebView, request: WebResourceRequest, response: WebResourceResponse) {
                    if (request.url.toString().startsWith(TERMINAL_URL.substringBeforeLast('/'))) events.trySend(TerminalRenderEvent.Failed(generation, "终端资源加载失败（${response.statusCode}）"))
                }
                override fun onReceivedSslError(view: WebView, handler: SslErrorHandler, error: android.net.http.SslError) {
                    handler.cancel(); events.trySend(TerminalRenderEvent.Failed(generation, "终端页面的安全连接失败"))
                }
                override fun onRenderProcessGone(view: WebView, detail: RenderProcessGoneDetail): Boolean {
                    currentFeed?.takeIf { it.active }?.let { currentFailure("终端渲染进程已停止，请重试") }
                    onRendererGone()
                    return true
                }
            }
            addTerminalBridge(bridge)
            loadUrl("$TERMINAL_URL?theme=${if (dark) "dark" else "light"}")
        }
    }
    SideEffect { webRef = web }
    LaunchedEffect(pageReady, dark) {
        web.setBackgroundColor(if (dark) android.graphics.Color.rgb(30, 30, 32) else android.graphics.Color.rgb(252, 252, 251))
        if (pageReady) web.evaluateJavascript("window.tty7Appearance($dark)", null)
    }
    LaunchedEffect(pageReady, generation, agentKind) {
        if (pageReady) web.evaluateJavascript("window.tty7AgentKind($generation,${JsonPrimitive(agentKind)})", null)
    }
    DisposableEffect(web) {
        onDispose { bridge.dispose(); web.stopLoading(); web.removeJavascriptInterface("Tty7"); web.destroy(); events.close() }
    }
    LaunchedEffect(synced, pageReady, feed) {
        if (synced && pageReady && feed?.active == true) web.evaluateJavascript("window.tty7Synced($generation)", null)
    }
    LaunchedEffect(focusRequest, web) {
        if (focusRequest > 0 && currentFeed?.synced == true) {
            focusManager.clearFocus(force = true)
            web.requestFocus()
            context.getSystemService(InputMethodManager::class.java).showSoftInput(web, InputMethodManager.SHOW_IMPLICIT)
        }
    }
    LaunchedEffect(web, feed) {
        if (feed != null && !pageReady) {
            try { withTimeout(15_000) { snapshotFlow { pageReady }.first { it } } }
            catch (e: kotlinx.coroutines.TimeoutCancellationException) {
                if (currentFeed === feed && feed.active) currentFailure("终端页面加载超时，请重试")
            }
        }
    }
    LaunchedEffect(feed, pageReady) {
        generation++
        val gen = generation
        historyJob?.cancel()
        historyJob = null
        historyBusy = false
        if (!pageReady) return@LaunchedEffect
        suspend fun nextEvent(): TerminalRenderEvent = withTimeout(15_000) {
            var event = events.receive()
            while (event.generation != gen) event = events.receive()
            event
        }
        try {
            web.evaluateJavascript("window.tty7Begin($gen)", null)
            when (val event = nextEvent()) {
                is TerminalRenderEvent.Begun -> Unit
                is TerminalRenderEvent.Failed -> throw IllegalStateException(event.message)
                else -> throw IllegalStateException("终端渲染器尚未就绪")
            }
            if (feed == null) return@LaunchedEffect
            var sequence = 1L
            while (feed.active) {
                val incoming = select<TerminalFrame?> {
                    feed.frames.onReceiveCatching { result -> result.exceptionOrNull()?.let { throw it }; result.getOrNull() }
                    events.onReceive { event ->
                        if (event.generation == gen && event is TerminalRenderEvent.Failed) throw IllegalStateException(event.message)
                        null
                    }
                } ?: if (feed.active) continue else break
                val frame = feed.stage(incoming) ?: continue
                val script = withContext(Dispatchers.Default) { "window.tty7Screen($gen,${frame.presentation()},$sequence)" }
                web.evaluateJavascript(script) { result ->
                    if (result != "true") events.trySend(TerminalRenderEvent.Failed(gen, "终端渲染调用失败"))
                }
                when (val event = nextEvent()) {
                    is TerminalRenderEvent.Rendered -> check(event.sequence == sequence) { "终端渲染顺序不一致" }
                    is TerminalRenderEvent.Failed -> throw IllegalStateException(event.message)
                    else -> throw IllegalStateException("终端页面意外重新加载")
                }
                feed.acknowledgeParsed(frame)
                sequence++
            }
        } catch (e: kotlinx.coroutines.TimeoutCancellationException) {
            if (gen == generation && feed?.active == true) { error = "终端渲染超时，请重试。电脑连接仍保留。"; currentFailure(error!!); feed.cancel() }
        } catch (e: kotlinx.coroutines.CancellationException) { throw e }
        catch (e: Exception) {
            if (gen == generation && feed?.active == true) { error = "终端渲染失败：${e.message ?: "渲染页面不可用"}"; currentFailure(error!!); feed.cancel() }
        }
    }
    LaunchedEffect(feed, pageReady, shortcuts) {
        shortcuts.collect { text ->
            if (pageReady && currentFeed === feed && feed?.synced == true) {
                web.evaluateJavascript("window.tty7Key($generation,${JsonPrimitive(text)})", null)
            }
        }
    }
    Box(Modifier.fillMaxSize()) {
        AndroidView(factory = { web }, modifier = Modifier.fillMaxSize())
        error?.let { Text(it) }
    }
}
