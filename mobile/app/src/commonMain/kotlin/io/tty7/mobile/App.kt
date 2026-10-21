package io.tty7.mobile

import androidx.compose.foundation.*
import androidx.compose.foundation.layout.*
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.foundation.lazy.items
import androidx.compose.foundation.text.BasicTextField
import androidx.compose.foundation.text.KeyboardActions
import androidx.compose.foundation.text.KeyboardOptions
import androidx.compose.material3.*
import androidx.compose.runtime.*
import androidx.compose.ui.Alignment
import androidx.compose.ui.draw.clip
import androidx.compose.ui.focus.onFocusChanged
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.ui.semantics.contentDescription
import androidx.compose.ui.semantics.clearAndSetSemantics
import androidx.compose.ui.input.pointer.pointerInput
import androidx.compose.ui.semantics.semantics
import androidx.compose.ui.semantics.Role
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.graphics.SolidColor
import androidx.compose.ui.geometry.Offset
import androidx.compose.ui.graphics.StrokeCap
import androidx.compose.ui.graphics.drawscope.Stroke
import androidx.compose.ui.Modifier
import androidx.compose.ui.platform.testTag
import androidx.compose.ui.platform.LocalFocusManager
import androidx.compose.ui.platform.LocalSoftwareKeyboardController
import androidx.compose.ui.text.font.FontFamily
import androidx.compose.ui.text.input.ImeAction
import androidx.compose.ui.text.style.TextOverflow
import androidx.compose.ui.unit.dp
import kotlinx.coroutines.flow.Flow
import kotlinx.coroutines.flow.MutableSharedFlow
import kotlinx.coroutines.launch

@Composable
fun MobileApp(
    platform: MobilePlatform, pairingLinks: Flow<String>,
    registerLifecycle: (((Boolean) -> Unit)?) -> Unit,
    registerNetwork: (((Boolean) -> Unit)?) -> Unit,
) {
    val scope = rememberCoroutineScope()
    val controller = remember(platform) { RemoteController(platform, scope) }
    val focusManager = LocalFocusManager.current
    val keyboard = LocalSoftwareKeyboardController.current
    val state by controller.state.collectAsState()
    var showSettings by remember { mutableStateOf(false) }
    var scanning by remember { mutableStateOf(false) }
    var showPairing by remember { mutableStateOf(false) }
    var pairingSubmitted by remember { mutableStateOf(false) }
    var search by remember { mutableStateOf("") }
    var qrText by remember { mutableStateOf("") }
    var drawerWorkspaceId by remember { mutableStateOf<String?>(null) }
    var workspaceMenu by remember { mutableStateOf(false) }
    var closing by remember { mutableStateOf<Pair<Workspace, Pane>?>(null) }
    var forgetting by remember { mutableStateOf<Computer?>(null) }
    val selectedPane = remember(state.terminalSelection) {
        state.terminalSelection?.let { it.first to it.second.id }
    }
    val selectedWorkspace = state.workspaces.firstOrNull { it.id == state.terminalSelection?.first }
    val currentPane = state.terminalSelection?.second?.let { selected ->
        selectedWorkspace?.panes?.firstOrNull { it.id == selected.id } ?: selected
    }
    val drawerWorkspace = state.workspaces.firstOrNull { it.id == drawerWorkspaceId }
        ?: selectedWorkspace ?: state.workspaces.firstOrNull()
    LaunchedEffect(state.terminalSelection?.first) { drawerWorkspaceId = state.terminalSelection?.first }
    val drawer = rememberDrawerState(DrawerValue.Closed)
    LaunchedEffect(drawer.targetValue) {
        if (drawer.targetValue == DrawerValue.Open) {
            focusManager.clearFocus(force = true)
            keyboard?.hide()
        }
    }
    LaunchedEffect(pairingLinks) {
        pairingLinks.collect {
            scanning = false
            showSettings = false
            showPairing = true
            pairingSubmitted = state.computers.isEmpty()
            scope.launch { drawer.close() }
            controller.pair(it)
        }
    }
    LaunchedEffect(state.connected) {
        if (state.connected && pairingSubmitted) {
            showPairing = false
            pairingSubmitted = false
        }
    }
    LaunchedEffect(state.connected, state.terminalSelection, showPairing) {
        if (state.connected && state.terminalSelection == null && !showPairing) drawer.open()
    }
    NavigationBackHandler(scanning || showPairing || drawer.isOpen || state.terminalSelection != null) {
        when {
            scanning -> scanning = false
            showPairing -> showPairing = false
            drawer.isOpen -> scope.launch { drawer.close() }
            state.terminalSelection != null -> scope.launch { drawer.open() }
        }
    }
    DisposableEffect(controller) {
        registerNetwork(controller::networkChanged)
        registerLifecycle(controller::setForeground)
        onDispose { registerLifecycle(null); registerNetwork(null); controller.setForeground(false) }
    }
    val systemDark = isSystemInDarkTheme()
    var theme by remember(platform) { mutableStateOf(platform.loadTheme()) }
    val dark = theme?.dark ?: systemDark
    ApplySystemAppearance(dark)
    MobileAppTheme(dark) {
        Surface(Modifier.fillMaxSize()) {
            if (scanning) {
                CameraScanner(onScan = { scanning = false; pairingSubmitted = true; controller.pair(it) }, onClose = { scanning = false })
            } else BoxWithConstraints(Modifier.fillMaxSize()) {
                val drawerWidth = minOf(360.dp, maxWidth * 0.86f)
                ModalNavigationDrawer(
                drawerState = drawer,
                // The terminal owns horizontal panning; opening is explicit.
                gesturesEnabled = drawer.isOpen,
                drawerContent = {
                    ModalDrawerSheet(Modifier.width(drawerWidth).fillMaxHeight(), drawerShape = RoundedCornerShape(0.dp), drawerContainerColor = MaterialTheme.colorScheme.surface) {
                        Column(Modifier.fillMaxSize().padding(horizontal = 12.dp).testTag("sessions-drawer")) {
                            Row(Modifier.fillMaxWidth(), verticalAlignment = Alignment.CenterVertically) {
                                Box(Modifier.weight(1f)) {
                                    Row(
                                        Modifier.heightIn(min = 48.dp).clickable(enabled = state.workspaces.size > 1) { workspaceMenu = true }.padding(start = 8.dp),
                                        verticalAlignment = Alignment.CenterVertically, horizontalArrangement = Arrangement.spacedBy(8.dp),
                                    ) {
                                        Text(drawerWorkspace?.name ?: "工作区", style = MaterialTheme.typography.titleLarge, maxLines = 1, overflow = TextOverflow.Ellipsis, modifier = Modifier.weight(1f, fill = false).testTag("workspace-title"))
                                        if (state.workspaces.size > 1) LineIcon("down")
                                    }
                                    DropdownMenu(expanded = workspaceMenu, onDismissRequest = { workspaceMenu = false }) {
                                        state.workspaces.forEach { workspace ->
                                            DropdownMenuItem(text = { Text(workspace.name) }, onClick = { drawerWorkspaceId = workspace.id; search = ""; workspaceMenu = false })
                                        }
                                    }
                                }
                                IconButton(onClick = { drawerWorkspace?.let { controller.createTerminal(it.id) } }, enabled = state.connected && drawerWorkspace != null, modifier = Modifier.testTag("create-terminal-${drawerWorkspace?.id}").semantics { contentDescription = "新建终端" }) { LineIcon("plus") }
                                IconButton(onClick = { scope.launch { drawer.close() } }, modifier = Modifier.testTag("close-sessions").semantics { contentDescription = "收起会话抽屉" }) { LineIcon("left") }
                            }
                            Box(Modifier.weight(1f)) {
                                SessionList(
                                    listOfNotNull(drawerWorkspace), selectedPane, search, { search = it }, state.connected,
                                    onSelect = { workspace, pane ->
                                        if (state.connected) {
                                            controller.selectTerminal(workspace.id, pane)
                                            scope.launch { drawer.close() }
                                        }
                                    },
                                    onClose = { workspace, pane -> if (state.connected) closing = workspace to pane },
                                )
                            }
                            HorizontalDivider()
                            TextButton(onClick = {
                                showSettings = true
                                scope.launch { drawer.close() }
                            }, modifier = Modifier.fillMaxWidth().testTag("open-settings-drawer")) { Text("设置") }
                            TextButton(onClick = {
                                controller.leaveTerminal()
                                showPairing = true
                                scope.launch { drawer.close() }
                            }, modifier = Modifier.fillMaxWidth().testTag("pairing-management")) { Text("扫描与配对管理") }
                        }
                    }
                },
            ) {
                Column(Modifier.fillMaxSize().safeDrawingPadding().imePadding().padding(horizontal = if (currentPane == null || showPairing) 12.dp else 0.dp)) {
                Row(Modifier.fillMaxWidth().heightIn(min = 52.dp).padding(horizontal = 4.dp), verticalAlignment = Alignment.CenterVertically, horizontalArrangement = Arrangement.spacedBy(4.dp)) {
                    IconButton(onClick = { scope.launch { drawer.open() } }, modifier = Modifier.testTag("toggle-sessions").semantics { contentDescription = "打开会话抽屉" }) { LineIcon("menu") }
                    Column(Modifier.weight(1f).padding(vertical = 6.dp), verticalArrangement = Arrangement.spacedBy(2.dp)) {
                        Text(
                            if (showPairing) "配对管理" else currentPane?.let(::terminalTitle) ?: drawerWorkspace?.name ?: "tty7",
                            style = MaterialTheme.typography.titleLarge, maxLines = 1, overflow = TextOverflow.Ellipsis,
                            modifier = Modifier.testTag("terminal-title"),
                        )
                        if (!showPairing) currentPane?.takeIf { !it.agentName.isNullOrBlank() }?.let { pane ->
                            Row(verticalAlignment = Alignment.CenterVertically, horizontalArrangement = Arrangement.spacedBy(6.dp)) {
                                Text(terminalAgentTitle(pane)!!, style = MaterialTheme.typography.labelLarge, maxLines = 1, overflow = TextOverflow.Ellipsis, modifier = Modifier.weight(1f, fill = false).testTag("terminal-agent-title"))
                                pane.agentStatus?.let { status ->
                                    AgentStatusDot(status)
                                    Text(agentStatus(status), style = MaterialTheme.typography.labelMedium, color = MaterialTheme.colorScheme.onSurfaceVariant, maxLines = 1, modifier = Modifier.testTag("agent-status-${pane.id}"))
                                }
                            }
                        }
                    }
                    IconButton(onClick = {
                        focusManager.clearFocus(force = true)
                        keyboard?.hide()
                        showSettings = true
                    }, modifier = Modifier.testTag("open-settings").semantics { contentDescription = "设置" }) { LineIcon("settings") }
                    if (!state.connected || state.status.startsWith("Terminal exited (")) Text(connectionStatus(state.status), style = MaterialTheme.typography.labelSmall, color = MaterialTheme.colorScheme.onSurfaceVariant, maxLines = 1, overflow = TextOverflow.Ellipsis, modifier = Modifier.widthIn(max = 112.dp).testTag("connection-status"))
                }
                state.verificationCode?.takeIf { state.computer?.pending == true }?.let {
                    Text("验证码：$it", style = MaterialTheme.typography.headlineSmall, fontFamily = FontFamily.Monospace, modifier = Modifier.testTag("verification-code"))
                    Text("请在电脑端核对验证码后批准连接。重新连接会生成新的验证码。")
                }
                state.error?.let { error ->
                    Text(connectionError(error, state.terminalSelection != null && !state.connected || state.reconnecting), color = MaterialTheme.colorScheme.error, modifier = Modifier.testTag("connection-error"))
                    TextButton(onClick = controller::clearError) { Text("关闭提示") }
                }
                if (showPairing || state.computer == null) {
                    Text("扫描与配对管理", style = MaterialTheme.typography.titleMedium)
                    val computer = state.computer
                    if (computer != null) {
                        Card(Modifier.fillMaxWidth().padding(vertical = 8.dp).testTag("paired-computer")) {
                            Column(Modifier.padding(16.dp)) {
                                Text(computer.hostName, style = MaterialTheme.typography.titleLarge)
                                Text(if (computer.pending) "等待电脑批准配对。" else "当前已配对的电脑。")
                                Text("一部手机只能配对一台电脑。如需扫描新的二维码，请先解除当前配对。")
                                TextButton(onClick = { forgetting = computer }, modifier = Modifier.testTag("unpair-computer")) { Text("解除配对") }
                            }
                        }
                        TextButton(onClick = { showPairing = false }, modifier = Modifier.testTag("cancel-pairing")) { Text("返回工作区与会话") }
                    } else {
                        Button(onClick = { scanning = true }, modifier = Modifier.testTag("scan-qr")) { Text("扫描电脑二维码") }
                        Text("在电脑的「配对设备」设置中显示二维码，也可以粘贴配对链接。")
                        OutlinedTextField(qrText, { qrText = it }, label = { Text("配对链接（tty7://pair?data=…）") }, modifier = Modifier.fillMaxWidth().testTag("pairing-uri"), singleLine = true)
                        TextButton(onClick = { pairingSubmitted = true; controller.pair(qrText); qrText = "" }, enabled = qrText.isNotBlank(), modifier = Modifier.testTag("pair-uri")) { Text("使用链接配对") }
                    }
                } else if (!state.connected && state.terminalSelection == null) {
                    val computer = state.computer
                    computer?.let {
                        Card(Modifier.fillMaxWidth().padding(vertical = 8.dp).testTag("saved-computer")) {
                            Column(Modifier.padding(16.dp)) {
                                Text(it.hostName, style = MaterialTheme.typography.titleLarge)
                                Text(when {
                                    it.pending -> "请在电脑端核对验证码并批准配对。"
                                    state.reconnecting -> "正在恢复与已授权电脑的连接…"
                                    state.status != "Disconnected" -> "正在连接已授权的电脑，请稍候…"
                                    else -> "此电脑已授权，无需重新扫描二维码。请确认电脑上的 tty7 正在运行且网络可用。"
                                })
                                Button(onClick = { controller.connect(it) }, modifier = Modifier.testTag(if (it.pending) "resume-pairing-${it.hostId}" else "connect-${it.hostId}")) {
                                    Text(if (it.pending) "重试配对" else "重新连接")
                                }
                            }
                        }
                    }
                    TextButton(onClick = { showPairing = true }, modifier = Modifier.testTag("pairing-management-root")) { Text("扫描与配对管理") }
                } else {
                    val selection = state.terminalSelection
                    val feed = state.terminal
                    val shortcuts = remember(feed) { MutableSharedFlow<String>(extraBufferCapacity = 16) }
                    val agentKind = agentAppearance(currentPane?.agentName)?.kind
                    val syncing = !state.connected || !state.terminalSynced || feed == null
                    val inputEnabled = !syncing
                    Column(Modifier.weight(1f).fillMaxWidth()) {
                        Box(Modifier.weight(1f).fillMaxWidth().background(MaterialTheme.colorScheme.surface).testTag("terminal-surface")) {
                            Box(Modifier.fillMaxSize().then(if (syncing) Modifier.clearAndSetSemantics {} else Modifier)) {
                                TerminalSurface(feed, inputEnabled, controller::input, controller::measuredPhoneGrid,
                                    { message -> feed?.let { controller.terminalFailed(it, message) } }, shortcuts, agentKind)
                            }
                            if (selection == null) {
                                Box(Modifier.fillMaxSize().background(MaterialTheme.colorScheme.background), contentAlignment = Alignment.Center) {
                                    Column(horizontalAlignment = Alignment.CenterHorizontally, verticalArrangement = Arrangement.spacedBy(12.dp)) {
                                        Text("选择一个会话开始", style = MaterialTheme.typography.titleLarge)
                                        Text("会话列表位于左侧抽屉。", color = MaterialTheme.colorScheme.onSurfaceVariant)
                                        Button(onClick = { scope.launch { drawer.open() } }) { Text("打开会话") }
                                    }
                                }
                            } else if (syncing) TerminalSyncOverlay(
                                reconnecting = !state.connected,
                                error = state.terminalError,
                                onRetry = controller::retryTerminal,
                            )
                        }
                        if (selection != null) {
                            TerminalKeyBar(
                                paneId = currentPane?.id,
                                agentKind = agentKind,
                                status = currentPane?.agentStatus,
                                enabled = inputEnabled,
                                onInput = { input -> if (inputEnabled) shortcuts.tryEmit(input) },
                            )
                        }
                    }
                }
                }
            }
            }
            if (showSettings) ThemeSettings(
                selected = if (dark) MobileTheme.Dark else MobileTheme.Light,
                onSelect = { selected -> platform.saveTheme(selected); theme = selected },
                onDismiss = { showSettings = false },
            )
            closing?.let { (workspace, pane) -> AlertDialog(onDismissRequest = { closing = null }, title = { Text("关闭终端？") }, text = { Text("确定关闭「${pane.title}」？电脑上此终端的 Shell 和正在运行的程序都将被终止。") }, confirmButton = { TextButton(onClick = { controller.closeTerminal(workspace.id, pane); closing = null }, modifier = Modifier.testTag("confirm-close-terminal")) { Text("关闭终端") } }, dismissButton = { TextButton(onClick = { closing = null }) { Text("取消") } }) }
            forgetting?.let { computer -> AlertDialog(onDismissRequest = { forgetting = null }, title = { Text("解除与「${computer.hostName}」的配对？") }, text = { Text("这会关闭手机上的连接并删除连接凭据，之后才能扫描二维码配对电脑。若要彻底撤销访问权限，请在电脑的「配对设备」设置中移除此设备。") }, confirmButton = { TextButton(onClick = { controller.forget(computer); forgetting = null; pairingSubmitted = false; qrText = ""; showPairing = true }, modifier = Modifier.testTag("confirm-unpair-computer")) { Text("解除配对") } }, dismissButton = { TextButton(onClick = { forgetting = null }) { Text("取消") } }) }
        }
    }
}

private data class TerminalKey(val tag: String, val label: String, val description: String, val input: String)

private val primaryTerminalKeys = listOf(
    TerminalKey("Enter", "回车", "回车 Enter，发送 CR", "\r"),
    TerminalKey("LF", "换行", "原地换行 LF / Ctrl-J", "\n"),
    TerminalKey("Esc", "Esc", "Esc，返回或取消，由终端程序决定", "\u001b"),
    TerminalKey("Ctrl-C", "Ctrl-C", "中断 Ctrl-C", "\u0003"),
)
private val directionTerminalKeys = listOf(
    TerminalKey("↑", "↑", "上方向键", "\u001b[A"),
    TerminalKey("↓", "↓", "下方向键", "\u001b[B"),
    TerminalKey("←", "←", "左方向键", "\u001b[D"),
    TerminalKey("→", "→", "右方向键", "\u001b[C"),
)
private val moreTerminalKeys = listOf(
    TerminalKey("Tab", "Tab · 制表", "制表 Tab", "\t"),
    TerminalKey("Home", "Home · 行首", "行首 Home", "\u001b[H"),
    TerminalKey("End", "End · 行尾", "行尾 End", "\u001b[F"),
)
private val codexShiftLeft = TerminalKey("Shift-Left", "Shift+←", "Codex Shift 加左方向键", "\u001b[1;2D")
private val rawLineFeed = TerminalKey("LF", "Ctrl-J", "发送 LF，由终端程序决定是否换行或执行", "\n")

@Composable
private fun TerminalKeyBar(paneId: Long?, agentKind: String?, status: String?, enabled: Boolean, onInput: (String) -> Unit) {
    var more by remember(paneId, agentKind, enabled) { mutableStateOf(false) }
    val waiting = status.equals("waiting", ignoreCase = true)
    Column(Modifier.fillMaxWidth()) {
        Row(Modifier.fillMaxWidth()) {
            primaryTerminalKeys.forEach { key ->
                val shown = if (key.tag == "LF" && agentKind != "codex" && agentKind != "omp") rawLineFeed else key
                TerminalKeyButton(shown, enabled, onInput, Modifier.weight(1f))
            }
            Box(Modifier.weight(1f)) {
                TextButton(
                    onClick = { more = true }, enabled = enabled,
                    contentPadding = PaddingValues(horizontal = 4.dp, vertical = 0.dp),
                    modifier = Modifier.fillMaxWidth().height(48.dp).testTag("key-more"),
                ) { Text("更多", style = MaterialTheme.typography.labelMedium, maxLines = 1) }
                DropdownMenu(expanded = more && enabled, onDismissRequest = { more = false }) {
                    if (agentKind == "codex") {
                        DropdownMenuItem(
                            text = { Text(codexShiftLeft.label) },
                            enabled = enabled,
                            onClick = { more = false; if (enabled) onInput(codexShiftLeft.input) },
                            modifier = Modifier.testTag("key-more-${codexShiftLeft.tag}").semantics { contentDescription = codexShiftLeft.description },
                        )
                    }
                    moreTerminalKeys.forEach { key ->
                        DropdownMenuItem(
                            text = { Text(key.label) },
                            enabled = enabled,
                            onClick = { more = false; if (enabled) onInput(key.input) },
                            modifier = Modifier.testTag("key-more-${key.tag}").semantics { contentDescription = key.description },
                        )
                    }
                }
            }
        }
        Row(
            Modifier.fillMaxWidth().testTag("terminal-direction-keys").semantics {
                contentDescription = if (waiting) "等待选择或确认：使用方向键选择，回车确认" else "使用方向键移动光标或选择"
            },
        ) {
            directionTerminalKeys.forEach { key ->
                TerminalKeyButton(key, enabled, onInput, Modifier.weight(1f))
            }
        }
    }
}

@Composable
private fun TerminalKeyButton(key: TerminalKey, enabled: Boolean, onInput: (String) -> Unit, modifier: Modifier) {
    TextButton(
        onClick = { if (enabled) onInput(key.input) }, enabled = enabled,
        contentPadding = PaddingValues(horizontal = 4.dp, vertical = 0.dp),
        modifier = modifier.height(48.dp).testTag("key-${key.tag}").semantics { contentDescription = key.description },
    ) { Text(key.label, style = MaterialTheme.typography.labelMedium, maxLines = 1) }
}

@Composable
private fun TerminalSyncOverlay(reconnecting: Boolean, error: String?, onRetry: () -> Unit) {
    val failed = error != null && !reconnecting
    Surface(Modifier.fillMaxSize().testTag("terminal-sync-overlay"), color = MaterialTheme.colorScheme.surface) {
        Box(Modifier.fillMaxSize()) {
            // A separate backdrop absorbs terminal touches without consuming
            // events intended for the foreground retry button.
            Box(Modifier.matchParentSize().pointerInput(Unit) {
                awaitPointerEventScope {
                    while (true) awaitPointerEvent().changes.forEach { it.consume() }
                }
            })
            Column(
                Modifier.fillMaxSize().padding(24.dp),
                horizontalAlignment = Alignment.CenterHorizontally,
                verticalArrangement = Arrangement.spacedBy(16.dp, Alignment.CenterVertically),
            ) {
                if (!failed) CircularProgressIndicator(Modifier.size(32.dp), strokeWidth = 3.dp)
                Text(when {
                    failed -> "终端显示暂不可用"
                    reconnecting -> "正在自动重连"
                    else -> "正在同步最新终端画面"
                }, style = MaterialTheme.typography.titleMedium)
                Text(
                    when {
                        failed -> connectionError(error!!)
                        reconnecting -> "网络连接中断，恢复后将自动加载最新内容。"
                        else -> "正在加载历史快照，完成后显示最新内容。"
                    },
                    style = MaterialTheme.typography.bodyMedium,
                    color = MaterialTheme.colorScheme.onSurfaceVariant,
                )
                if (failed) Button(onClick = onRetry, modifier = Modifier.testTag("retry-terminal")) { Text("重试加载") }
            }
        }
    }
}

@Composable
private fun SessionList(
    workspaces: List<Workspace>, selectedPane: Pair<String, Long>?, search: String, onSearch: (String) -> Unit, connected: Boolean,
    onSelect: (Workspace, Pane) -> Unit, onClose: (Workspace, Pane) -> Unit,
) {
    val query = search.trim()
    Column(Modifier.fillMaxSize()) {
        SessionSearchField(search, onSearch)
        LazyColumn(Modifier.weight(1f).testTag("session-list")) {
            if (workspaces.isEmpty()) item { Text(if (connected) "暂无工作区，请在电脑端创建。" else "连接电脑后显示工作区。", style = MaterialTheme.typography.bodyMedium, color = MaterialTheme.colorScheme.onSurfaceVariant, modifier = Modifier.padding(12.dp)) }
            workspaces.forEach { workspace ->
                val sections = paneSections(workspace.panes)
                val visible = sections.map { section ->
                    section.copy(panes = section.panes.filter { pane ->
                        query.isEmpty() || listOfNotNull(workspace.name, pane.title, pane.cwd, pane.group, pane.gitBranch, pane.agentName).any { it.contains(query, ignoreCase = true) }
                    })
                }.filter { it.panes.isNotEmpty() }
                if (query.isEmpty() || visible.isNotEmpty()) {
                    if (workspace.panes.isEmpty()) item(key = "empty-${workspace.id}") {
                        Text("暂无终端，点击右上角 + 新建。", style = MaterialTheme.typography.bodyMedium, color = MaterialTheme.colorScheme.onSurfaceVariant, modifier = Modifier.padding(12.dp))
                    }
                    visible.forEachIndexed { index, section ->
                        val branches = section.panes.mapNotNull { it.gitBranch }.distinct()
                        val sharedBranch = if (section.name != null && branches.size == 1) branches.single() else null
                        item(key = "group-${workspace.id}-$index") {
                            Row(
                                Modifier.fillMaxWidth().padding(top = 12.dp, bottom = 4.dp, start = 12.dp, end = 12.dp).testTag("session-group-${workspace.id}-$index"),
                                horizontalArrangement = Arrangement.spacedBy(8.dp), verticalAlignment = Alignment.CenterVertically,
                            ) {
                                Text(section.title, style = MaterialTheme.typography.labelLarge, color = MaterialTheme.colorScheme.onSurfaceVariant, maxLines = 1, overflow = TextOverflow.Ellipsis, modifier = Modifier.weight(1f))
                                sharedBranch?.let { Text(it, style = MaterialTheme.typography.labelSmall, color = MaterialTheme.colorScheme.onSurfaceVariant, maxLines = 1, overflow = TextOverflow.Ellipsis, modifier = Modifier.widthIn(max = 112.dp)) }
                            }
                        }
                        items(section.panes, key = { "pane-${workspace.id}-${it.id}" }) { pane ->
                            val selected = selectedPane == (workspace.id to pane.id)
                            Row(
                                Modifier.fillMaxWidth().padding(vertical = 2.dp).background(if (selected) MaterialTheme.colorScheme.secondaryContainer else Color.Transparent, RoundedCornerShape(6.dp)),
                                verticalAlignment = Alignment.CenterVertically,
                            ) {
                                TextButton(onClick = { onSelect(workspace, pane) }, enabled = connected, shape = RoundedCornerShape(6.dp), contentPadding = PaddingValues(12.dp), modifier = Modifier.weight(1f).testTag("terminal-${pane.id}")) {
                                    Row(Modifier.fillMaxWidth(), verticalAlignment = Alignment.CenterVertically, horizontalArrangement = Arrangement.spacedBy(12.dp)) {
                                        val agent = pane.agentName != null || pane.agentStatus != null
                                        SessionAvatar(pane)
                                        Column(Modifier.weight(1f), verticalArrangement = Arrangement.spacedBy(4.dp)) {
                                            Text(pane.title, style = MaterialTheme.typography.titleSmall, color = MaterialTheme.colorScheme.onSurface, maxLines = 1, overflow = TextOverflow.Ellipsis)
                                            if (agent) AgentBadge(pane)
                                            if (sharedBranch == null) pane.gitBranch?.let { Text("分支：$it", style = MaterialTheme.typography.bodySmall, maxLines = 1, overflow = TextOverflow.Ellipsis, color = MaterialTheme.colorScheme.onSurfaceVariant) }
                                            if (sharedBranch == null && pane.gitBranch == null) pane.cwd?.let { Text(it, style = MaterialTheme.typography.bodySmall, maxLines = 1, overflow = TextOverflow.Ellipsis, color = MaterialTheme.colorScheme.onSurfaceVariant) }
                                        }
                                    }
                                }
                                SessionActions(pane, connected, onClose = { onClose(workspace, pane) })
                            }
                        }
                    }
                }
            }
            if (query.isNotEmpty() && workspaces.none { workspace ->
                workspace.panes.any { pane -> listOfNotNull(workspace.name, pane.title, pane.cwd, pane.group, pane.gitBranch, pane.agentName).any { it.contains(query, ignoreCase = true) } }
            }) item { Text("没有匹配的会话。", modifier = Modifier.padding(vertical = 16.dp)) }
        }
    }
}

@Composable
private fun SessionSearchField(search: String, onSearch: (String) -> Unit) {
    val focusManager = LocalFocusManager.current
    val keyboard = LocalSoftwareKeyboardController.current
    var focused by remember { mutableStateOf(false) }
    val shape = RoundedCornerShape(10.dp)
    BasicTextField(
        value = search,
        onValueChange = onSearch,
        singleLine = true,
        textStyle = MaterialTheme.typography.bodyMedium.copy(color = MaterialTheme.colorScheme.onSurface),
        cursorBrush = SolidColor(MaterialTheme.colorScheme.primary),
        keyboardOptions = KeyboardOptions(imeAction = ImeAction.Search),
        keyboardActions = KeyboardActions(onSearch = { focusManager.clearFocus(); keyboard?.hide() }),
        modifier = Modifier.fillMaxWidth().padding(top = 4.dp, bottom = 4.dp)
            .heightIn(min = 44.dp).onFocusChanged { focused = it.isFocused }
            .clip(shape).background(MaterialTheme.colorScheme.surfaceContainerHighest)
            .border(1.dp, if (focused) MaterialTheme.colorScheme.outline else Color.Transparent, shape)
            .testTag("search-sessions").semantics { contentDescription = "搜索会话" },
        decorationBox = { innerTextField ->
            Row(
                Modifier.padding(start = 12.dp),
                verticalAlignment = Alignment.CenterVertically,
                horizontalArrangement = Arrangement.spacedBy(8.dp),
            ) {
                LineIcon("search", MaterialTheme.colorScheme.onSurfaceVariant)
                Box(Modifier.weight(1f).padding(vertical = 10.dp)) {
                    if (search.isEmpty()) Text("搜索会话", style = MaterialTheme.typography.bodyMedium, color = MaterialTheme.colorScheme.onSurfaceVariant)
                    innerTextField()
                }
                if (search.isNotEmpty()) {
                    Box(
                        Modifier.size(44.dp).clip(RoundedCornerShape(22.dp)).clickable(role = Role.Button) { onSearch("") }
                            .testTag("clear-session-search").semantics { contentDescription = "清空搜索" },
                        contentAlignment = Alignment.Center,
                    ) { LineIcon("close", MaterialTheme.colorScheme.onSurfaceVariant) }
                } else Spacer(Modifier.width(12.dp))
            }
        },
    )
}

@Composable
private fun SessionActions(pane: Pane, connected: Boolean, onClose: () -> Unit) {
    var expanded by remember { mutableStateOf(false) }
    LaunchedEffect(connected) { if (!connected) expanded = false }
    Box {
        IconButton(
            onClick = { expanded = true }, enabled = connected,
            modifier = Modifier.testTag("session-actions-${pane.id}").semantics { contentDescription = "会话操作：${pane.title}" },
        ) { LineIcon("more", MaterialTheme.colorScheme.onSurfaceVariant) }
        DropdownMenu(expanded = expanded, onDismissRequest = { expanded = false }) {
            DropdownMenuItem(
                text = { Text("关闭终端", color = MaterialTheme.colorScheme.error) },
                leadingIcon = { LineIcon("close", MaterialTheme.colorScheme.error) },
                enabled = connected,
                onClick = { expanded = false; onClose() },
                modifier = Modifier.testTag("close-terminal-${pane.id}").semantics { contentDescription = "关闭终端：${pane.title}" },
            )
        }
    }
}

@Composable
private fun AgentBadge(pane: Pane) {
    Row(verticalAlignment = Alignment.CenterVertically, horizontalArrangement = Arrangement.spacedBy(6.dp)) {
        pane.agentStatus?.let { AgentStatusDot(it) }
        Text(
            listOfNotNull(pane.agentName, pane.agentStatus?.let(::agentStatus)).joinToString(" · "),
            style = MaterialTheme.typography.labelSmall, color = MaterialTheme.colorScheme.onSurfaceVariant,
            maxLines = 1, overflow = TextOverflow.Ellipsis,
            modifier = Modifier.testTag("agent-status-${pane.id}"),
        )
    }
}

@Composable
private fun SessionAvatar(pane: Pane) {
    val look = agentAppearance(pane.agentName)
    if (look == null) {
        LineIcon("terminal", MaterialTheme.colorScheme.onSurfaceVariant)
        return
    }
    val mark = remember(look.kind) { agentMark(look.kind) }
    Box(
        Modifier.size(24.dp).background(Color(0xFF000000 or look.field.toLong()), RoundedCornerShape(6.dp))
            .testTag("agent-icon-${pane.id}").semantics { contentDescription = pane.agentName ?: "Agent" },
        contentAlignment = Alignment.Center,
    ) {
        val ink = Color(0xFF000000 or look.ink.toLong())
        if (mark != null) Icon(mark, contentDescription = null, tint = ink, modifier = Modifier.size(18.dp))
        else LineIcon("bot", ink)
    }
}

@Composable
private fun AgentStatusDot(status: String) {
    val color = when (status.lowercase()) {
        "working", "running" -> Color(0xFF3B82F6)
        "waiting" -> Color(0xFFF59E0B)
        "done" -> Color(0xFF22C55E)
        else -> return
    }
    Canvas(Modifier.size(8.dp)) {
        if (status.equals("waiting", ignoreCase = true)) drawCircle(color, style = Stroke(width = 1.5.dp.toPx()))
        else drawCircle(color)
    }
}

/** Draw controls independently of platform glyph coverage. The button owns its label. */
@Composable
private fun LineIcon(name: String, color: Color = LocalContentColor.current) {
    Canvas(Modifier.size(20.dp)) {
        val unit = size.width / 24f
        fun line(x1: Float, y1: Float, x2: Float, y2: Float) = drawLine(
            color, Offset(x1 * unit, y1 * unit), Offset(x2 * unit, y2 * unit),
            strokeWidth = 1.8f * unit, cap = StrokeCap.Round,
        )
        when (name) {
            "settings" -> {
                drawCircle(color, radius = 3f * unit, center = Offset(12f * unit, 12f * unit), style = Stroke(1.8f * unit))
                drawCircle(color, radius = 7f * unit, center = Offset(12f * unit, 12f * unit), style = Stroke(1.8f * unit))
                line(12f, 2f, 12f, 5f); line(12f, 19f, 12f, 22f)
                line(2f, 12f, 5f, 12f); line(19f, 12f, 22f, 12f)
                line(5f, 5f, 7f, 7f); line(17f, 17f, 19f, 19f)
                line(5f, 19f, 7f, 17f); line(17f, 7f, 19f, 5f)
            }
            "menu" -> { line(4f, 6f, 20f, 6f); line(4f, 12f, 20f, 12f); line(4f, 18f, 20f, 18f) }
            "close" -> { line(6f, 6f, 18f, 18f); line(18f, 6f, 6f, 18f) }
            "left" -> { line(14f, 6f, 8f, 12f); line(8f, 12f, 14f, 18f) }
            "more" -> { for (x in listOf(5f, 12f, 19f)) drawCircle(color, radius = 1.5f * unit, center = Offset(x * unit, 12f * unit)) }
            "plus" -> { line(12f, 5f, 12f, 19f); line(5f, 12f, 19f, 12f) }
            "down" -> { line(7f, 10f, 12f, 15f); line(12f, 15f, 17f, 10f) }
            "search" -> { drawCircle(color, radius = 6f * unit, center = Offset(10f * unit, 10f * unit), style = Stroke(1.8f * unit)); line(15f, 15f, 20f, 20f) }
            "terminal" -> { line(5f, 7f, 10f, 12f); line(10f, 12f, 5f, 17f); line(13f, 17f, 20f, 17f) }
            "bot" -> {
                line(12f, 3f, 12f, 6f)
                drawRoundRect(color, Offset(4f * unit, 6f * unit), androidx.compose.ui.geometry.Size(16f * unit, 14f * unit), androidx.compose.ui.geometry.CornerRadius(3f * unit), style = Stroke(1.8f * unit))
                drawCircle(color, 1.2f * unit, Offset(8f * unit, 12f * unit))
                drawCircle(color, 1.2f * unit, Offset(16f * unit, 12f * unit))
                line(9f, 16f, 15f, 16f)
            }
        }
    }
}

internal fun terminalTitle(pane: Pane): String =
    pane.group?.takeIf { it.isNotBlank() && it != "Ungrouped" } ?: "未分组"

internal fun terminalAgentTitle(pane: Pane): String? = pane.agentName?.takeIf { it.isNotBlank() }?.let {
    pane.title.takeIf { title -> title.isNotBlank() && title != "-" } ?: it
}

internal data class PaneSection(val name: String?, val panes: List<Pane>) {
    val title: String get() = if (name == null || name == "Ungrouped") "未分组" else name
}

// The desktop supplies sections in visual order, including pinned groups.
// Do not infer groups from cwd or sort labels: both lose user-defined grouping.
internal fun paneSections(panes: List<Pane>): List<PaneSection> {
    val sections = mutableListOf<PaneSection>()
    var start = 0
    while (start < panes.size) {
        val group = panes[start].group
        val identity = panes[start].groupId ?: group
        var end = start + 1
        while (end < panes.size && (panes[end].groupId ?: panes[end].group) == identity) end++
        sections.add(PaneSection(group, panes.subList(start, end)))
        start = end
    }
    return sections
}

private fun agentStatus(status: String): String = when (status.lowercase()) {
    "working", "running" -> "正在运行"
    "waiting" -> "等待选择或确认"
    "done" -> "已完成"
    "idle" -> "空闲"
    else -> "状态：$status"
}

private fun connectionStatus(status: String): String = when (status) {
    "Disconnected" -> "未连接"
    "Connecting" -> "正在连接"
    "Reconnecting" -> "正在重新连接"
    "Waiting for desktop approval" -> "等待电脑批准"
    "Authenticating" -> "正在验证身份"
    "Compare code on desktop, then approve" -> "请在电脑端核对验证码并批准"
    "Connected" -> "已连接"
    else -> if (status.startsWith("Terminal exited (")) "终端已退出（${status.removePrefix("Terminal exited (").removeSuffix(")")}）" else status
}

private fun connectionError(error: String, automaticReconnect: Boolean = false): String = when (error) {
    "Connect to a computer first" -> "请先连接电脑。"
    "Disconnected", "Not connected", "Connection closed", "Socket closed", "Relay connection ended" ->
        if (automaticReconnect) "连接已中断，正在自动重连。" else "连接已关闭，请重试连接。"
    "Not authenticated" -> "尚未完成身份验证，请稍候或重新连接。"
    "Pairing expired", "Pairing QR expired. Scan a new QR on the computer.", "Pairing QR expired or invalid. Generate a new QR on desktop." -> "配对已过期或无效，请在电脑端生成新的二维码。"
    "Pairing rejected" -> "电脑已拒绝配对。"
    "Authentication failed" -> "身份验证失败，请检查电脑端的设备授权。"
    "Terminal not found" -> "找不到此终端，请从会话列表重新选择。"
    "terminal observer disconnected unexpectedly" -> "终端输出订阅意外中断，请重试加载当前终端。"
    "Connection timed out. Reconnect or scan a new pairing QR." -> "连接超时，请重试连接。若尚未完成配对，请扫描电脑端的新二维码。"
    "Expected tty7://pair URI", "Missing pairing data", "Invalid pairing identity", "Invalid host identity", "Invalid base64url" -> "配对链接无效，请扫描电脑端的二维码或粘贴完整的 tty7://pair 链接。"
    "Pairing URI too large" -> "配对链接过长，请使用电脑端生成的链接。"
    "Unsupported pairing version" -> "不支持此配对版本，请更新手机与电脑上的 tty7。"
    "Unsupported mobile protocol. Update phone and computer." -> "远程协议版本不匹配，请同时更新手机 APP 和电脑端 tty7。"
    "Verification code mismatch" -> "验证码不匹配，请重新配对，并在电脑端仔细核对验证码。"
    "TLS certificate pin is required", "Invalid TLS SHA256 fingerprint" -> "配对链接缺少有效的 TLS 证书指纹，请重新生成配对二维码。"
    "Invalid relay URL", "Relay must use wss", "Unencrypted WebSocket is only allowed for loopback in debug builds" -> "中继地址无效，远程连接必须使用安全的 WSS 地址。"
    UNPAIR_BEFORE_PAIRING -> "请先在「扫描与配对管理」中解除当前电脑的配对，再扫描或使用新的配对链接。"
    "Desktop took back terminal size control. Reopen the terminal to use the phone grid." -> "电脑已接管终端尺寸。重新打开此会话可再次使用手机尺寸。"
    "terminal size control was taken back" -> "电脑已接管终端尺寸。重新打开此会话可再次使用手机尺寸。"
    "Could not securely save paired computers" -> "无法安全保存电脑的连接凭据，请检查手机存储空间。"
    "Relay refused connection" -> "中继服务拒绝了连接，请检查电脑与网络后重试。"
    else -> if (error.contains(" timed out; the remote operation may still complete. Refresh to reconcile its outcome.")) {
        val operation = when (error.substringBefore(" timed out;")) {
            "subscribe" -> "打开终端"
            "unsubscribe" -> "返回会话列表"
            "list" -> "刷新会话"
            "input" -> "发送终端输入"
            "terminal_size" -> "调整终端尺寸"
            "create_workspace" -> "创建工作区"
            "create_terminal" -> "创建终端"
            "close_terminal" -> "关闭终端"
            else -> "远程操作"
        }
        "$operation 超时，但电脑可能仍会完成操作。会话列表会自动更新，请确认结果。"
    } else "连接或操作失败：$error"
}
