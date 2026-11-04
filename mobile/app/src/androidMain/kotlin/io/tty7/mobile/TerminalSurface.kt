package io.tty7.mobile

import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.material3.Text
import androidx.compose.runtime.*
import androidx.compose.ui.Modifier
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.viewinterop.AndroidView
import kotlinx.coroutines.CancellationException
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.Job
import kotlinx.coroutines.currentCoroutineContext
import kotlinx.coroutines.ensureActive
import kotlinx.coroutines.flow.Flow
import kotlinx.coroutines.launch
import kotlinx.coroutines.withContext
import kotlinx.serialization.json.boolean
import kotlinx.serialization.json.int
import kotlinx.serialization.json.jsonObject
import kotlinx.serialization.json.jsonPrimitive

@Composable
actual fun TerminalSurface(feed: TerminalFeed?, synced: Boolean, onInput: (String) -> Unit, onResize: (TerminalFeed?, Int, Int) -> Unit, onFailure: (String) -> Unit,
    shortcuts: Flow<String>, agentKind: String?) {
    val context = LocalContext.current
    val dark = LocalMobileDarkTheme.current
    val scope = rememberCoroutineScope()
    val currentFeed by rememberUpdatedState(feed)
    val currentInput by rememberUpdatedState(onInput)
    val currentSynced by rememberUpdatedState(synced)
    val currentFailure by rememberUpdatedState(onFailure)
    val currentResize by rememberUpdatedState(onResize)
    val view = remember(context) { NativeTerminalView(context) }
    var generation by remember { mutableLongStateOf(0) }
    var historyJob by remember { mutableStateOf<Job?>(null) }
    var error by remember(feed) { mutableStateOf<String?>(null) }

    SideEffect {
        view.onInput = { text -> if (currentFeed?.synced == true) currentInput(text) }
        val target = feed
        val gen = generation
        view.onResize = { cols, rows ->
            if (gen == generation && currentFeed === target && target?.active != false) currentResize(target, cols, rows)
        }
        view.reportGridSize()
        view.onHistory = { cursor ->
            val target = currentFeed
            val gen = generation
            if (target?.synced == true && historyJob?.isActive != true) {
                historyJob = scope.launch {
                    try {
                        val page = target.history(cursor)
                        val (rows, sizes) = withContext(Dispatchers.Default) {
                            val decoded = ArrayList<NativeTerminalRow>(page.lines.size)
                            val bytes = ArrayList<Int>(page.lines.size)
                            for (value in page.lines) {
                                val line = value.jsonObject
                                val ansi = line.text("ansi")
                                decoded.add(NativeTerminalDecoder.row(ansi, line.getValue("cols").jsonPrimitive.int,
                                    line["wrapped"]?.jsonPrimitive?.boolean ?: false,
                                    line["wrap_padding"]?.jsonPrimitive?.boolean ?: false))
                                bytes.add(ansi.encodeToByteArray().size + line.text("text").encodeToByteArray().size)
                            }
                            decoded to bytes
                        }
                        if (gen == generation && currentFeed === target && target.active) view.applyHistory(page, rows, sizes)
                    } catch (e: CancellationException) { throw e }
                    catch (e: Exception) {
                        if (gen == generation && currentFeed === target && target.active) view.historyFailed(e.message ?: "历史加载失败")
                    }
                }
            }
        }
        view.setAppearance(dark)
        view.setAgentKind(agentKind)
        view.setInputEnabled(synced && feed?.synced == true)
    }
    DisposableEffect(view) {
        onDispose { historyJob?.cancel(); view.dispose() }
    }
    LaunchedEffect(feed, view) {
        generation++
        historyJob?.cancel()
        historyJob = null
        view.reset()
        val gen = generation
        view.onResize = { cols, rows ->
            if (gen == generation && currentFeed === feed && feed?.active != false) currentResize(feed, cols, rows)
        }
        view.reportGridSize()
        if (feed == null) return@LaunchedEffect
        try {
            for (incoming in feed.frames) {
                if (!feed.active) break
                val frame = feed.stage(incoming) ?: continue
                val rows = withContext(Dispatchers.Default) {
                    frame.lines.map { line -> line.row to NativeTerminalDecoder.row(line.ansi, frame.cols, line.wrapped, line.wrapPadding) }
                }
                currentCoroutineContext().ensureActive()
                if (currentFeed !== feed || !feed.active) break
                // Commit the complete canonical state before returning protocol credit.
                // Drawing is scheduled on vsync, never a prerequisite for consuming the next patch.
                view.applyScreen(frame, rows)
                feed.acknowledgeParsed(frame)
                view.setInputEnabled(currentSynced && feed.synced)
            }
        } catch (e: CancellationException) { throw e }
        catch (e: Exception) {
            if (currentFeed === feed && feed.active) {
                error = "终端渲染失败：${e.message ?: "画面不可用"}"
                view.setInputEnabled(false)
                currentFailure(error!!)
                feed.cancel()
            }
        }
    }
    LaunchedEffect(view, shortcuts) {
        shortcuts.collect { text ->
            if (currentFeed?.synced == true) view.sendShortcut(text)
        }
    }
    Box(Modifier.fillMaxSize()) {
        AndroidView(factory = { view }, modifier = Modifier.fillMaxSize())
        error?.let { Text(it) }
    }
}
