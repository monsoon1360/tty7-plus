package io.tty7.mobile

import androidx.compose.foundation.*
import androidx.compose.foundation.layout.*
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.foundation.lazy.items
import androidx.compose.material3.*
import androidx.compose.runtime.*
import androidx.compose.ui.Alignment
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.foundation.shape.CircleShape
import androidx.compose.ui.draw.clip
import androidx.compose.ui.semantics.contentDescription
import androidx.compose.ui.semantics.clearAndSetSemantics
import androidx.compose.ui.input.pointer.pointerInput
import androidx.compose.ui.semantics.semantics
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.Modifier
import androidx.compose.ui.platform.testTag
import androidx.compose.ui.platform.LocalFocusManager
import androidx.compose.ui.platform.LocalSoftwareKeyboardController
import androidx.compose.ui.text.font.FontFamily
import androidx.compose.ui.text.style.TextOverflow
import androidx.compose.ui.unit.dp
import kotlinx.coroutines.flow.Flow
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
    var scanning by remember { mutableStateOf(false) }
    var showPairing by remember { mutableStateOf(false) }
    var pairingSubmitted by remember { mutableStateOf(false) }
    var search by remember { mutableStateOf("") }
    var qrText by remember { mutableStateOf("") }
    var workspaceName by remember { mutableStateOf("") }
    var createWorkspace by remember { mutableStateOf(false) }
    var closing by remember { mutableStateOf<Pair<Workspace, Pane>?>(null) }
    var forgetting by remember { mutableStateOf<Computer?>(null) }
    val selectedPane = remember(state.terminalSelection) {
        state.terminalSelection?.let { it.first to it.second.id }
    }
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
    val colors = if (isSystemInDarkTheme()) darkColorScheme(
        primary = Color(0xFF6BE5A2), primaryContainer = Color(0xFF244A36), onPrimaryContainer = Color(0xFFB0F1CC),
        secondaryContainer = Color(0xFF244A36), surfaceContainerHighest = Color(0xFF343936),
        background = Color(0xFF17171A), surface = Color(0xFF202023),
    ) else lightColorScheme(
        primary = Color(0xFF176B43), primaryContainer = Color(0xFFDCF1E3), onPrimaryContainer = Color(0xFF134A31),
        secondaryContainer = Color(0xFFDCF1E3), surfaceContainerHighest = Color(0xFFE7E9E4),
        background = Color(0xFFF6F5F1), surface = Color(0xFFF6F5F1),
    )
    MaterialTheme(colorScheme = colors) {
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
                    ModalDrawerSheet(Modifier.width(drawerWidth).fillMaxHeight()) {
                        Column(Modifier.fillMaxSize().padding(16.dp).testTag("sessions-drawer")) {
                            Row(Modifier.fillMaxWidth(), verticalAlignment = Alignment.CenterVertically) {
                                Text("工作区与会话", style = MaterialTheme.typography.titleLarge, modifier = Modifier.weight(1f))
                                IconButton(onClick = { scope.launch { drawer.close() } }, modifier = Modifier.testTag("close-sessions").semantics { contentDescription = "收起会话抽屉" }) { Text("×") }
                            }
                            Text(state.computer?.hostName ?: "尚未连接电脑", style = MaterialTheme.typography.bodyMedium, color = MaterialTheme.colorScheme.onSurfaceVariant)
                            Row(Modifier.fillMaxWidth(), horizontalArrangement = Arrangement.SpaceBetween) {
                                TextButton(onClick = controller::refresh, enabled = state.connected, modifier = Modifier.testTag("refresh-sessions")) { Text("刷新") }
                                TextButton(onClick = { createWorkspace = true }, enabled = state.connected, modifier = Modifier.testTag("create-workspace")) { Text("新建工作区") }
                            }
                            Box(Modifier.weight(1f)) {
                                SessionList(
                                    state.workspaces, selectedPane, search, { search = it },
                                    onSelect = { workspace, pane ->
                                        if (state.connected) {
                                            controller.selectTerminal(workspace.id, pane)
                                            scope.launch { drawer.close() }
                                        }
                                    },
                                    onCreate = { if (state.connected) controller.createTerminal(it) },
                                    onClose = { workspace, pane -> if (state.connected) closing = workspace to pane },
                                )
                            }
                            HorizontalDivider()
                            TextButton(onClick = {
                                controller.leaveTerminal()
                                showPairing = true
                                scope.launch { drawer.close() }
                            }, modifier = Modifier.fillMaxWidth().testTag("pairing-management")) { Text("扫描与配对管理") }
                        }
                    }
                },
            ) {
                Column(Modifier.fillMaxSize().safeDrawingPadding().imePadding().padding(12.dp)) {
                Row(Modifier.fillMaxWidth(), horizontalArrangement = Arrangement.SpaceBetween, verticalAlignment = Alignment.CenterVertically) {
                    IconButton(onClick = { scope.launch { drawer.open() } }, modifier = Modifier.testTag("toggle-sessions").semantics { contentDescription = "打开会话抽屉" }) { Text("☰", style = MaterialTheme.typography.titleLarge) }
                    Column(Modifier.weight(1f)) {
                        Row(verticalAlignment = Alignment.CenterVertically, horizontalArrangement = Arrangement.spacedBy(6.dp)) {
                            Text("●", color = if (state.connected) MaterialTheme.colorScheme.primary else MaterialTheme.colorScheme.onSurfaceVariant, style = MaterialTheme.typography.labelSmall)
                            Text(
                                listOfNotNull(state.computer?.hostName, connectionStatus(state.status)).joinToString(" · "),
                                style = MaterialTheme.typography.labelMedium, color = MaterialTheme.colorScheme.onSurfaceVariant,
                                maxLines = 1, overflow = TextOverflow.Ellipsis, modifier = Modifier.testTag("connection-status"),
                            )
                        }
                    }
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
                    val selected = selection?.second
                    val pane = selected?.let { selectedPane ->
                        state.workspaces.firstOrNull { it.id == selection?.first }?.panes?.firstOrNull { it.id == selectedPane.id } ?: selectedPane
                    }
                    val syncing = !state.connected || !state.terminalSynced || feed == null
                    val inputEnabled = !syncing
                    Column(Modifier.weight(1f).fillMaxWidth()) {
                        if (pane != null) {
                            Row(Modifier.fillMaxWidth().padding(vertical = 6.dp), verticalAlignment = Alignment.CenterVertically, horizontalArrangement = Arrangement.spacedBy(8.dp)) {
                                Text(pane.title, style = MaterialTheme.typography.labelLarge, maxLines = 1, overflow = TextOverflow.Ellipsis, modifier = Modifier.weight(1f))
                                if (pane.agentName != null || pane.agentStatus != null) AgentBadge(pane)
                            }
                        }
                        Box(Modifier.weight(1f).fillMaxWidth().clip(RoundedCornerShape(12.dp)).background(Color(0xFF17171A)).testTag("terminal-surface")) {
                            Box(Modifier.fillMaxSize().then(if (syncing) Modifier.clearAndSetSemantics {} else Modifier)) {
                                TerminalSurface(feed, inputEnabled, controller::input, controller::measuredPhoneGrid,
                                    { message -> feed?.let { controller.terminalFailed(it, message) } })
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
                            Row(Modifier.horizontalScroll(rememberScrollState())) {
                                listOf(
                                    Triple("Enter", "回车", "\r"), Triple("Ctrl-C", "中断", "\u0003"),
                                    Triple("Tab", "制表", "\t"), Triple("Esc", "退出键", "\u001b"),
                                    Triple("↑", "↑", "\u001b[A"), Triple("↓", "↓", "\u001b[B"),
                                    Triple("←", "←", "\u001b[D"), Triple("→", "→", "\u001b[C"),
                                ).forEach { (tag, label, input) ->
                                    TextButton(onClick = { controller.input(input) }, enabled = inputEnabled, contentPadding = PaddingValues(horizontal = 12.dp, vertical = 0.dp), modifier = Modifier.height(36.dp).testTag("key-$tag")) { Text(label, style = MaterialTheme.typography.labelMedium) }
                                }
                            }
                        }
                    }
                }
                }
            }
            }
            if (createWorkspace) AlertDialog(onDismissRequest = { createWorkspace = false }, title = { Text("新建工作区") }, text = {
                OutlinedTextField(workspaceName, { workspaceName = it }, label = { Text("名称（可选）") }, modifier = Modifier.testTag("workspace-name"))
            }, confirmButton = { TextButton(onClick = { controller.createWorkspace(workspaceName); workspaceName = ""; createWorkspace = false }) { Text("创建") } }, dismissButton = { TextButton(onClick = { createWorkspace = false }) { Text("取消") } })
            closing?.let { (workspace, pane) -> AlertDialog(onDismissRequest = { closing = null }, title = { Text("关闭终端？") }, text = { Text("确定关闭「${pane.title}」？电脑上此终端的 Shell 和正在运行的程序都将被终止。") }, confirmButton = { TextButton(onClick = { controller.closeTerminal(workspace.id, pane); closing = null }, modifier = Modifier.testTag("confirm-close-terminal")) { Text("关闭终端") } }, dismissButton = { TextButton(onClick = { closing = null }) { Text("取消") } }) }
            forgetting?.let { computer -> AlertDialog(onDismissRequest = { forgetting = null }, title = { Text("解除与「${computer.hostName}」的配对？") }, text = { Text("这会关闭手机上的连接并删除连接凭据，之后才能扫描二维码配对电脑。若要彻底撤销访问权限，请在电脑的「配对设备」设置中移除此设备。") }, confirmButton = { TextButton(onClick = { controller.forget(computer); forgetting = null; pairingSubmitted = false; qrText = ""; showPairing = true }, modifier = Modifier.testTag("confirm-unpair-computer")) { Text("解除配对") } }, dismissButton = { TextButton(onClick = { forgetting = null }) { Text("取消") } }) }
        }
    }
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
    workspaces: List<Workspace>, selectedPane: Pair<String, Long>?, search: String, onSearch: (String) -> Unit,
    onSelect: (Workspace, Pane) -> Unit, onCreate: (String) -> Unit, onClose: (Workspace, Pane) -> Unit,
) {
    val query = search.trim()
    Column(Modifier.fillMaxSize()) {
        TextField(
            search, onSearch, placeholder = { Text("搜索会话、目录、分组或分支") }, singleLine = true,
            shape = RoundedCornerShape(16.dp),
            colors = TextFieldDefaults.colors(
                focusedIndicatorColor = Color.Transparent, unfocusedIndicatorColor = Color.Transparent,
                focusedContainerColor = MaterialTheme.colorScheme.surfaceContainerHighest,
                unfocusedContainerColor = MaterialTheme.colorScheme.surfaceContainerHighest,
            ),
            modifier = Modifier.fillMaxWidth().padding(vertical = 8.dp).testTag("search-sessions"),
        )
        LazyColumn(Modifier.weight(1f).testTag("session-list")) {
            if (workspaces.isEmpty()) item { Text("暂无会话，请新建工作区。", modifier = Modifier.padding(vertical = 16.dp)) }
            workspaces.forEach { workspace ->
                val sections = paneSections(workspace.panes)
                val visible = sections.map { section ->
                    section.copy(panes = section.panes.filter { pane ->
                        query.isEmpty() || listOfNotNull(workspace.name, pane.title, pane.cwd, pane.group, pane.gitBranch, pane.agentName).any { it.contains(query, ignoreCase = true) }
                    })
                }.filter { it.panes.isNotEmpty() }
                if (query.isEmpty() || visible.isNotEmpty()) {
                    item(key = "workspace-${workspace.id}") {
                        Row(Modifier.fillMaxWidth().padding(top = 8.dp), horizontalArrangement = Arrangement.SpaceBetween, verticalAlignment = Alignment.CenterVertically) {
                            Text(workspace.name, style = MaterialTheme.typography.titleMedium, modifier = Modifier.weight(1f))
                            TextButton(onClick = { onCreate(workspace.id) }, modifier = Modifier.testTag("create-terminal-${workspace.id}")) { Text("新建终端") }
                        }
                    }
                    visible.forEachIndexed { index, section ->
                        val branches = section.panes.mapNotNull { it.gitBranch }.distinct()
                        val sharedBranch = if (section.name != null && branches.size == 1) branches.single() else null
                        item(key = "group-${workspace.id}-$index") {
                            Row(
                                Modifier.fillMaxWidth().padding(top = 16.dp, bottom = 4.dp, start = 12.dp, end = 12.dp).testTag("session-group-${workspace.id}-$index"),
                                horizontalArrangement = Arrangement.spacedBy(8.dp), verticalAlignment = Alignment.CenterVertically,
                            ) {
                                Text(section.title, style = MaterialTheme.typography.labelLarge, color = MaterialTheme.colorScheme.onSurfaceVariant, maxLines = 1, overflow = TextOverflow.Ellipsis, modifier = Modifier.weight(1f))
                                sharedBranch?.let { Text(it, style = MaterialTheme.typography.labelSmall, color = MaterialTheme.colorScheme.onSurfaceVariant, maxLines = 1, overflow = TextOverflow.Ellipsis, modifier = Modifier.widthIn(max = 160.dp)) }
                            }
                        }
                        items(section.panes, key = { "pane-${workspace.id}-${it.id}" }) { pane ->
                            val selected = selectedPane == (workspace.id to pane.id)
                            Row(
                                Modifier.fillMaxWidth().padding(vertical = 2.dp).background(if (selected) MaterialTheme.colorScheme.secondaryContainer else MaterialTheme.colorScheme.surface, RoundedCornerShape(12.dp)),
                                verticalAlignment = Alignment.CenterVertically,
                            ) {
                                TextButton(onClick = { onSelect(workspace, pane) }, contentPadding = PaddingValues(12.dp), modifier = Modifier.weight(1f).testTag("terminal-${pane.id}")) {
                                    Row(Modifier.fillMaxWidth(), verticalAlignment = Alignment.CenterVertically, horizontalArrangement = Arrangement.spacedBy(12.dp)) {
                                        val agent = pane.agentName != null || pane.agentStatus != null
                                        Box(
                                            Modifier.size(32.dp).background(if (agent) MaterialTheme.colorScheme.primaryContainer else MaterialTheme.colorScheme.surfaceContainerHighest, CircleShape),
                                            contentAlignment = Alignment.Center,
                                        ) {
                                            Text(if (agent) "π" else "›_", fontFamily = FontFamily.Monospace, style = MaterialTheme.typography.labelLarge, color = if (agent) MaterialTheme.colorScheme.onPrimaryContainer else MaterialTheme.colorScheme.onSurfaceVariant)
                                        }
                                        Column(Modifier.weight(1f), verticalArrangement = Arrangement.spacedBy(4.dp)) {
                                            Text(pane.title, style = MaterialTheme.typography.titleSmall, color = MaterialTheme.colorScheme.onSurface, maxLines = 1, overflow = TextOverflow.Ellipsis)
                                            if (agent) AgentBadge(pane)
                                            if (sharedBranch == null) pane.gitBranch?.let { Text("分支：$it", style = MaterialTheme.typography.bodySmall, maxLines = 1, overflow = TextOverflow.Ellipsis, color = MaterialTheme.colorScheme.onSurfaceVariant) }
                                            if (sharedBranch == null && pane.gitBranch == null) pane.cwd?.let { Text(it, style = MaterialTheme.typography.bodySmall, maxLines = 1, overflow = TextOverflow.Ellipsis, color = MaterialTheme.colorScheme.onSurfaceVariant) }
                                        }
                                    }
                                }
                                IconButton(
                                    onClick = { onClose(workspace, pane) },
                                    modifier = Modifier.testTag("close-terminal-${pane.id}").semantics { contentDescription = "关闭终端：${pane.title}" },
                                ) { Text("×", style = MaterialTheme.typography.titleLarge, color = MaterialTheme.colorScheme.onSurfaceVariant) }
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
private fun AgentBadge(pane: Pane) {
    val working = pane.agentStatus.equals("working", ignoreCase = true)
    val color = if (working) MaterialTheme.colorScheme.primary else MaterialTheme.colorScheme.onSurfaceVariant
    Surface(color = color.copy(alpha = 0.09f), shape = RoundedCornerShape(6.dp)) {
        Text(
            listOfNotNull(pane.agentName, pane.agentStatus?.let(::agentStatus)).joinToString(" · "),
            style = MaterialTheme.typography.labelSmall, color = color,
            maxLines = 1, overflow = TextOverflow.Ellipsis,
            modifier = Modifier.widthIn(max = 164.dp).padding(horizontal = 6.dp, vertical = 3.dp).testTag("agent-status-${pane.id}"),
        )
    }
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
    "working", "running" -> "● 正在运行"
    "waiting" -> "○ 等待操作"
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
    "Terminal not found" -> "找不到此终端，请刷新会话列表。"
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
        "$operation 超时，但电脑可能仍会完成操作。请刷新会话列表确认结果。"
    } else "连接或操作失败：$error"
}
