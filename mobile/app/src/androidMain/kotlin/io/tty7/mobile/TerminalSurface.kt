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
import kotlinx.coroutines.channels.Channel
import kotlinx.coroutines.withTimeout
import kotlinx.coroutines.selects.select
import kotlinx.serialization.json.JsonPrimitive
import java.io.ByteArrayInputStream

private const val TERMINAL_URL = "https://appassets.androidplatform.net/assets/terminal/index.html"
private sealed interface TerminalRenderEvent {
    data object Ready : TerminalRenderEvent
    data class Rendered(val sequence: Long) : TerminalRenderEvent
    data class Failed(val message: String) : TerminalRenderEvent
}

private class TerminalBridge(
    private val onInput: (String) -> Unit,
    private val onResize: (Int, Int) -> Unit,
    private val onFocusInput: () -> Unit,
    private val events: Channel<TerminalRenderEvent>,
) {
    private val main = Handler(Looper.getMainLooper())
    @Volatile private var disposed = false
    @JavascriptInterface fun input(text: String) {
        if (!disposed && text.length <= 65_536) main.post { if (!disposed) onInput(text) }
    }
    @JavascriptInterface fun resize(cols: Int, rows: Int) {
        if (!disposed && cols in 20..1000 && rows in 5..1000) main.post { if (!disposed) onResize(cols, rows) }
    }
    @JavascriptInterface fun focusInput() {
        if (!disposed) main.post { if (!disposed) onFocusInput() }
    }
    @JavascriptInterface fun ready() { if (!disposed) events.trySend(TerminalRenderEvent.Ready) }
    @JavascriptInterface fun rendered(sequence: Long) { if (!disposed) events.trySend(TerminalRenderEvent.Rendered(sequence)) }
    @JavascriptInterface fun failed(message: String) { if (!disposed) events.trySend(TerminalRenderEvent.Failed(message.take(1024))) }
    fun dispose() { disposed = true; main.removeCallbacksAndMessages(null) }
}
@SuppressLint("SetJavaScriptEnabled")
@Composable
actual fun TerminalSurface(feed: TerminalFeed, synced: Boolean, onInput: (String) -> Unit, onResize: (Int, Int) -> Unit, onFailure: (String) -> Unit) {
    val context = LocalContext.current
    val focusManager = LocalFocusManager.current
    val currentInput by rememberUpdatedState(onInput)
    val currentResize by rememberUpdatedState(onResize)
    val events = remember(feed) { Channel<TerminalRenderEvent>(Channel.UNLIMITED) }
    val feedFailure = remember(feed) { onFailure }
    var error by remember(feed) { mutableStateOf<String?>(null) }
    var pageReady by remember(feed) { mutableStateOf(false) }
    var focusRequest by remember(feed) { mutableIntStateOf(0) }
    val bridge = remember(feed) {
        TerminalBridge(
            { if (feed.synced) currentInput(it) },
            { cols, rows -> if (feed.synced) currentResize(cols, rows) },
            { if (feed.synced) focusRequest++ },
            events,
        )
    }
    val web = remember(feed) {
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
            setBackgroundColor(android.graphics.Color.rgb(16, 16, 20))
            webViewClient = object : WebViewClient() {
                override fun shouldOverrideUrlLoading(view: WebView, request: WebResourceRequest): Boolean = true
                override fun shouldInterceptRequest(view: WebView, request: WebResourceRequest): WebResourceResponse {
                    return assets.shouldInterceptRequest(request.url) ?: WebResourceResponse("text/plain", "UTF-8", 403, "Forbidden", emptyMap(), ByteArrayInputStream(ByteArray(0)))
                }
                override fun onReceivedError(view: WebView, request: WebResourceRequest, error: WebResourceError) {
                    if (request.url.toString().startsWith(TERMINAL_URL.substringBeforeLast('/'))) {
                        events.trySend(TerminalRenderEvent.Failed("终端页面加载失败：${error.description}"))
                    }
                }
                override fun onReceivedHttpError(view: WebView, request: WebResourceRequest, response: WebResourceResponse) {
                    if (request.url.toString().startsWith(TERMINAL_URL.substringBeforeLast('/'))) {
                        events.trySend(TerminalRenderEvent.Failed("终端资源加载失败（${response.statusCode}）"))
                    }
                }
                override fun onReceivedSslError(view: WebView, handler: SslErrorHandler, error: android.net.http.SslError) {
                    handler.cancel()
                    events.trySend(TerminalRenderEvent.Failed("终端页面的安全连接失败"))
                }
                override fun onRenderProcessGone(view: WebView, detail: RenderProcessGoneDetail): Boolean {
                    events.trySend(TerminalRenderEvent.Failed("终端渲染进程已停止，请重试"))
                    return true
                }
            }
            addJavascriptInterface(bridge, "Tty7")
            loadUrl(TERMINAL_URL)
        }
    }
    DisposableEffect(web) {
        onDispose { feed.cancel(); bridge.dispose(); web.stopLoading(); web.removeJavascriptInterface("Tty7"); web.destroy(); events.close() }
    }
    LaunchedEffect(synced, pageReady, web) {
        if (synced && pageReady) {
            web.evaluateJavascript("window.tty7Synced()", null)
        }
    }
    LaunchedEffect(focusRequest, web) {
        if (focusRequest > 0 && feed.synced) {
            // DOM focus alone cannot reclaim the IME after the Compose editor.
            // Only the terminal's classified tap/click requests native focus.
            focusManager.clearFocus(force = true)
            web.requestFocus()
            context.getSystemService(InputMethodManager::class.java)
                .showSoftInput(web, InputMethodManager.SHOW_IMPLICIT)
        }
    }
    LaunchedEffect(feed, web) {
        try {
            when (val event = withTimeout(15_000) { events.receive() }) {
                TerminalRenderEvent.Ready -> pageReady = true
                is TerminalRenderEvent.Failed -> throw IllegalStateException(event.message)
                is TerminalRenderEvent.Rendered -> throw IllegalStateException("终端渲染器尚未就绪")
            }
            var sequence = 1L
            while (true) {
                val frame = select<TerminalFrame?> {
                    feed.frames.onReceiveCatching { result ->
                        result.exceptionOrNull()?.let { throw it }
                        result.getOrNull()
                    }
                    events.onReceive { event ->
                        throw IllegalStateException(
                            if (event is TerminalRenderEvent.Failed) event.message else "终端渲染器状态异常"
                        )
                    }
                } ?: break
                val call = "window.tty7Write(${JsonPrimitive(frame.data)},${frame.snapshot},${frame.cols},${frame.rows},$sequence)"
                web.evaluateJavascript(call) { result ->
                    if (result != "true") events.trySend(TerminalRenderEvent.Failed("终端渲染调用失败"))
                }
                when (val event = withTimeout(15_000) { events.receive() }) {
                    is TerminalRenderEvent.Rendered -> check(event.sequence == sequence) { "终端渲染顺序不一致" }
                    is TerminalRenderEvent.Failed -> throw IllegalStateException(event.message)
                    TerminalRenderEvent.Ready -> throw IllegalStateException("终端页面意外重新加载")
                }
                feed.acknowledgeParsed(frame)
                sequence++
            }
        } catch (e: kotlinx.coroutines.TimeoutCancellationException) {
            if (feed.active) {
                val message = "终端渲染超时，请重试。电脑连接仍保留。"
                error = message
                feedFailure(message)
                feed.cancel()
            }
        } catch (e: kotlinx.coroutines.CancellationException) { throw e }
        catch (e: Exception) {
            if (feed.active) {
                val message = "终端渲染失败：${e.message ?: "渲染页面不可用"}"
                error = message
                feedFailure(message)
                feed.cancel()
            }
        }
    }
    Box(Modifier.fillMaxSize()) {
        if (error == null) AndroidView(factory = { web }, modifier = Modifier.fillMaxSize())
        error?.let { Text(it) }
    }
}
