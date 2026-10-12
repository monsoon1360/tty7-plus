package io.tty7.mobile

import kotlinx.coroutines.*
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.asStateFlow
import kotlinx.coroutines.sync.Mutex
import kotlinx.coroutines.sync.withLock
import kotlinx.serialization.json.*

data class RemoteState(
    val computers: List<Computer> = emptyList(), val computer: Computer? = null,
    val status: String = "Disconnected", val connected: Boolean = false,
    val verificationCode: String? = null, val workspaces: List<Workspace> = emptyList(),
    val terminal: TerminalFeed? = null, val terminalSelection: Pair<String, Pane>? = null,
    val terminalSynced: Boolean = false, val terminalError: String? = null, val error: String? = null,
    val reconnecting: Boolean = false,
)
class RemoteController(
    private val platform: MobilePlatform,
    private val scope: CoroutineScope,
    private val worker: CoroutineDispatcher = Dispatchers.Default,
    private val handshakeTimeout: Long = 10_000,
    private val requestTimeout: Long = 15_000,
) {
    private val mutable = MutableStateFlow(RemoteState())
    val state = mutable.asStateFlow()
    private var connection: Job? = null
    private var socket: RemoteSocket? = null
    private var crypto: SecureSession? = null
    private val sending = Mutex()
    private val storage = Mutex()
    private var loaded = false
    private val lifetime = ForegroundLifetime()
    private val actions = mutableSetOf<Job>()
    private val retiredRequests = RetiredRequestIds()
    private val switching = Mutex()
    private var nextRequest = 1L
    private val requests = mutableMapOf<Long, CompletableDeferred<JsonElement>>()
    private val pendingSubscriptions = mutableMapOf<Long, TerminalFeed>()
    private var restoring: Job? = null
    private var automaticConnection = true
    private var networkAvailable = true
    private var selectionIntent = 0L
    private var selectionAction: Job? = null
    // A canceled subscribe can still succeed remotely, so retain its pane until unsubscribe completes.
    private var subscribedPane: Long? = null
    private var failedTerminal: TerminalFeed? = null
    private val refreshing = Mutex()
    private data class AgentReading(val revision: ULong, val name: String?, val status: String?) {
        fun apply(pane: Pane) = pane.copy(agentName = name, agentStatus = status, agentRevision = revision)
    }
    private val agentReadings = mutableMapOf<Long, AgentReading>()
    private fun fail(error: Throwable) {
        mutable.value = mutable.value.copy(error = error.message ?: "Remote connection failed")
    }
    private fun action(block: suspend () -> Unit): Job? {
        if (!lifetime.active) return null
        val generation = lifetime.generation
        val job = scope.launch(start = CoroutineStart.LAZY) {
            try { block() }
            catch (e: TimeoutCancellationException) { if (lifetime.accepts(generation)) fail(IllegalStateException("Operation timed out", e)) }
            catch (e: CancellationException) { throw e }
            catch (e: Exception) { if (lifetime.accepts(generation)) fail(e) }
        }
        actions.add(job)
        job.invokeOnCompletion { actions.remove(job) }
        job.start()
        return job
    }
    private suspend fun ensureLoaded() = storage.withLock {
        if (!loaded) {
            val stored = platform.loadComputers()
            currentCoroutineContext().ensureActive()
            val computers = listOfNotNull(retainedComputer(stored))
            withContext(NonCancellable) {
                if (computers != stored) platform.saveComputers(computers)
                mutable.value = mutable.value.copy(computers = computers, computer = computers.singleOrNull())
                loaded = true
            }
        }
    }
    private suspend fun remember(computer: Computer, newPair: Boolean = false) {
        ensureLoaded()
        storage.withLock {
            currentCoroutineContext().ensureActive()
            // A completed commit and its in-memory publication are one transaction.
            // Handshakes and reconnects deliberately remain cancellable outside it.
            withContext(NonCancellable) {
                val saved = updatedComputer(mutable.value.computers.singleOrNull(), computer, newPair)
                val computers = listOf(saved)
                platform.saveComputers(computers)
                mutable.value = mutable.value.copy(computers = computers, computer = saved)
            }
        }
    }
    fun pair(uri: String) {
        action {
            val generation = lifetime.generation
            ensureLoaded()
            if (!lifetime.accepts(generation)) return@action
            if (mutable.value.computers.isNotEmpty()) {
                fail(IllegalStateException(UNPAIR_BEFORE_PAIRING))
                return@action
            }
            val computer = platform.parseQr(uri)
            remember(computer, newPair = true)
            currentCoroutineContext().ensureActive()
            if (lifetime.accepts(generation)) connect(computer)
        }
    }
    fun setForeground(foreground: Boolean) {
        if (lifetime.active == foreground) return
        lifetime.setForeground(foreground)
        if (foreground) restoreConnection() else stopConnection()
    }
    private fun restoreConnection(allowPending: Boolean = false) {
        restoring = action {
            val generation = lifetime.generation
            ensureLoaded()
            if (networkAvailable && automaticConnection && lifetime.accepts(generation) && connection?.isActive != true) {
                val saved = mutable.value.computer?.takeUnless { it.pending && !allowPending }
                saved?.let { connect(it) }
            }
        }
    }
    // A true notification also denotes a changed default route, even when both routes have INTERNET.
    fun networkChanged(available: Boolean) {
        if (!available && !networkAvailable) return
        networkAvailable = available
        if (!lifetime.active || !automaticConnection) return
        stopConnection()
        mutable.value = mutable.value.copy(status = "Reconnecting", reconnecting = true)
        if (available) restoreConnection(allowPending = true)
    }
    fun connect(computer: Computer) {
        if (!lifetime.active) return
        if (mutable.value.computers.singleOrNull() != computer) {
            fail(IllegalStateException(UNPAIR_BEFORE_PAIRING))
            return
        }
        automaticConnection = true
        restoring?.cancel()
        lifetime.invalidate()
        val generation = lifetime.generation
        invalidateTerminalIntent()
        mutable.value.terminal?.cancel()
        mutable.value = mutable.value.copy(terminal = null, terminalSynced = false)
        val previous = connection
        previous?.cancel()
        socket?.close()
        mutable.value = mutable.value.copy(computer = computer, connected = false, status = "Connecting", error = null,
            verificationCode = computer.verificationCode, reconnecting = true)
        if (!networkAvailable) {
            mutable.value = mutable.value.copy(status = "Reconnecting")
            return
        }
        connection = scope.launch {
            previous?.join()
            currentCoroutineContext().ensureActive()
            var target = computer
            var retryDelay = 1_000L
            while (lifetime.accepts(generation)) {
                mutable.value = mutable.value.copy(computer = target, status = "Connecting", reconnecting = true)
                val approved = runConnection(target, generation)
                currentCoroutineContext().ensureActive()
                if (!lifetime.accepts(generation)) break
                if (approved != null) {
                    target = approved
                    continue
                }
                if (target.pending) break
                mutable.value = mutable.value.copy(status = "Reconnecting", reconnecting = true)
                delay(retryDelay)
                retryDelay = (retryDelay * 2).coerceAtMost(30_000L)
            }
        }
    }
    private suspend fun runConnection(computer: Computer, generation: Long): Computer? {
            var polling: Job? = null
            var approved: Computer? = null
            try {
                ensureLoaded()
                currentCoroutineContext().ensureActive()
                check(lifetime.accepts(generation)) { "Connection attempt is no longer foreground" }
                require(!computer.pending || platform.nowSeconds() < (computer.expiresAt ?: 0)) { "Pairing QR expired. Scan a new QR on the computer." }
                val remote = platform.connect(computer)
                if (!lifetime.accepts(generation) || !currentCoroutineContext().isActive) { remote.close(); throw CancellationException("Connection attempt stopped") }
                socket = remote
                remote.send(obj("type" to JsonPrimitive("register"), "role" to JsonPrimitive("client"), "host_id" to JsonPrimitive(computer.hostId)).toString())
                val ready = withTimeout(handshakeTimeout) { wireJson.parseToJsonElement(remote.receive()).jsonObject }
                require(ready.optionalText("type") == "ready") { ready.optionalText("message") ?: "Relay refused connection" }
                val clientNonce = platform.randomNonce()
                sendInner(obj("kind" to JsonPrimitive("hello"), "protocol" to JsonPrimitive(MOBILE_PROTOCOL),
                    "credential_id" to JsonPrimitive(computer.credentialId), "nonce" to JsonPrimitive(clientNonce)))
                val challenge = withTimeout(handshakeTimeout) { receiveInner(remote) }
                if (challenge.optionalText("kind") == "error" && challenge.optionalText("code") == "update_required") {
                    error("Unsupported mobile protocol. Update phone and computer.")
                }
                require(challenge.text("kind") == "challenge") { "Invalid host challenge" }
                val protocol = challenge["protocol"]?.jsonPrimitive
                require(protocol != null && !protocol.isString && protocol.intOrNull == MOBILE_PROTOCOL) {
                    "Unsupported mobile protocol. Update phone and computer."
                }
                crypto = withContext(worker) { platform.deriveSession(computer.secret, clientNonce, challenge.text("nonce")) }
                mutable.value = mutable.value.copy(verificationCode = crypto!!.verificationCode, status = if (computer.pending) "Waiting for desktop approval" else "Authenticating")
                sendEncrypted(if (computer.pending) obj("type" to JsonPrimitive("pair"), "device_name" to JsonPrimitive(platform.deviceName)) else obj("type" to JsonPrimitive("auth")))
                var authenticated = false
                while (currentCoroutineContext().isActive) {
                    val envelope = if (authenticated) receiveInner(remote) else withTimeout(if (computer.pending) 310_000L else 15_000L) { receiveInner(remote) }
                    val session = crypto ?: error("Encryption session missing")
                    val message = withContext(worker) { wireJson.parseToJsonElement(session.decrypt(envelope)).jsonObject }
                    when (message.text("type")) {
                        "pair_pending" -> {
                            require(computer.pending && !authenticated) { "Unexpected pairing response" }
                            val code = message.text("code")
                            require(code == crypto!!.verificationCode) { "Verification code mismatch" }
                            remember(computer.copy(verificationCode = code))
                            currentCoroutineContext().ensureActive()
                            mutable.value = mutable.value.copy(status = "Compare code on desktop, then approve", verificationCode = code)
                        }
                        "paired" -> {
                            require(computer.pending && !authenticated) { "Unexpected credentials" }
                            val saved = computer.copy(credentialId = message.text("credential_id"), secret = message.text("secret"), expiresAt = null, verificationCode = null)
                            require(!saved.pending && saved.credentialId.isNotBlank()) { "Invalid device credential" }
                            // Validate the received PSK before persisting it.
                            withContext(worker) { platform.deriveSession(saved.secret, clientNonce, challenge.text("nonce")) }
                            remember(saved)
                            currentCoroutineContext().ensureActive()
                            approved = saved
                            break
                        }
                        "pair_rejected" -> error(message.optionalText("message") ?: "Pairing rejected")
                        "authenticated" -> {
                            require(!computer.pending && !authenticated) { "Unexpected authentication" }
                            authenticated = true
                            val renamed = computer.copy(hostName = message.text("host_name"))
                            remember(renamed)
                            currentCoroutineContext().ensureActive()
                            mutable.value = mutable.value.copy(connected = true, status = "Connected", error = null, reconnecting = false)
                            refresh()
                            if (mutable.value.terminalError == null) {
                                mutable.value.terminalSelection?.let { (workspace, pane) -> selectTerminal(workspace, pane) }
                            }
                            polling = scope.launch {
                                while (lifetime.accepts(generation)) {
                                    delay(2_000)
                                    refresh()
                                }
                            }
                        }
                        "response" -> {
                            require(authenticated) { "Operation before authentication" }
                            val id = message.getValue("id").jsonPrimitive.long
                            val pending = requests[id]
                            if (pending == null) {
                                require(retiredRequests.consume(id)) { "Unexpected response ID" }
                                continue
                            }
                            val ok = message.getValue("ok").jsonPrimitive.boolean
                            val result = message["result"] ?: JsonNull
                            val subscribing = pendingSubscriptions[id]
                            if (ok && subscribing != null && mutable.value.terminal === subscribing && subscribing.active) {
                                val subscription = result.jsonObject
                                require(subscription.getValue("cols").jsonPrimitive.int in 1..1000 &&
                                    subscription.getValue("rows").jsonPrimitive.int in 1..1000) { "Invalid terminal dimensions" }
                                val subscriptionId = subscription.getValue("subscription_id").jsonPrimitive
                                require(subscriptionId.isString) { "Invalid terminal subscription" }
                                // Install the generation before the receive loop can observe its first frame.
                                subscribing.bindSubscription(subscriptionId.content)
                            }
                            requests.remove(id)
                            pendingSubscriptions.remove(id)
                            if (ok) pending.complete(result)
                            else pending.completeExceptionally(IllegalStateException(message.optionalText("error") ?: "Operation failed"))
                        }
                        "screen" -> {
                            require(authenticated) { "Screen before authentication" }
                            val feed = mutable.value.terminal
                            if (feed != null && feed.matches(message.getValue("pane_id").jsonPrimitive.long, message.text("subscription_id"))) {
                                try {
                                    feed.enqueue(TerminalFrame.fromJson(message))
                                } catch (e: Exception) {
                                    currentCoroutineContext().ensureActive()
                                    if (mutable.value.terminal === feed && feed.active) throw e
                                }
                            }
                        }
                        "terminal_lease" -> {
                            require(authenticated) { "Terminal lease before authentication" }
                            val feed = mutable.value.terminal
                            if (feed != null && feed.matches(message.getValue("pane_id").jsonPrimitive.long, message.text("subscription_id"))) {
                                val held = message.getValue("held").jsonPrimitive.boolean
                                feed.updateSizeLease(held)
                                if (!held) mutable.value = mutable.value.copy(
                                    error = "Desktop took back terminal size control. Reopen the terminal to use the phone grid.",
                                )
                            }
                        }
                        "agent" -> {
                            require(authenticated) { "Agent status before authentication" }
                            val feed = mutable.value.terminal
                            if (feed != null && feed.matches(message.getValue("pane_id").jsonPrimitive.long, message.text("subscription_id"))) {
                                val agent = message["agent"]?.takeUnless { it is JsonNull }?.jsonObject
                                val reading = AgentReading(message.text("agent_revision").toULong(),
                                    agent?.optionalText("name"), agent?.optionalText("status"))
                                val previous = agentReadings[feed.pane.id]
                                if (previous == null || reading.revision >= previous.revision) {
                                    agentReadings[feed.pane.id] = reading
                                    feed.updatePane(reading.apply(feed.pane))
                                    val selected = mutable.value.terminalSelection?.let { (workspace, pane) ->
                                        workspace to if (pane.id == feed.pane.id) reading.apply(pane) else pane
                                    }
                                    mutable.value = mutable.value.copy(
                                        workspaces = mutable.value.workspaces.map { workspace ->
                                            workspace.copy(panes = workspace.panes.map { pane ->
                                                if (pane.id == feed.pane.id) reading.apply(pane) else pane
                                            })
                                        },
                                        terminalSelection = selected,
                                    )
                                }
                            }
                        }
                        "exited" -> {
                            require(authenticated) { "Terminal exit before authentication" }
                            val feed = mutable.value.terminal ?: failedTerminal
                            if (feed != null && feed.identifiesSubscription(message.getValue("pane_id").jsonPrimitive.long, message.text("subscription_id"))) {
                                clearTerminalSelection()
                                mutable.value = mutable.value.copy(status = "Terminal exited (${message["code"] ?: "unknown"})")
                                refresh()
                            }
                        }
                        "terminal_error" -> {
                            require(authenticated) { "Terminal error before authentication" }
                            val feed = mutable.value.terminal
                            if (feed != null && feed.matches(message.getValue("pane_id").jsonPrimitive.long, message.text("subscription_id"))) {
                                terminalFailed(feed, message.text("message"))
                            }
                        }
                        else -> error("Unsupported host message")
                    }
                }
            } catch (e: TimeoutCancellationException) { fail(IllegalStateException("Connection timed out. Reconnect or scan a new pairing QR.", e)) }
            catch (e: CancellationException) { throw e }
            catch (e: Exception) { fail(e) }
            finally {
                polling?.cancel()
                socket?.close(); socket = null; crypto = null
                requests.values.forEach { it.completeExceptionally(IllegalStateException("Disconnected")) }; requests.clear()
                pendingSubscriptions.clear()
                retiredRequests.clear()
                agentReadings.clear()
                subscribedPane = null
                failedTerminal = null
                invalidateTerminalIntent()
                mutable.value.terminal?.close()
                val waitingForNetwork = lifetime.active && automaticConnection && !networkAvailable
                mutable.value = mutable.value.copy(connected = false, terminal = null, terminalSynced = false,
                    status = if (waitingForNetwork) "Reconnecting" else "Disconnected", reconnecting = waitingForNetwork)
            }
            return approved
    }
    private suspend fun receiveInner(remote: RemoteSocket): JsonObject {
        val outer = wireJson.parseToJsonElement(remote.receive()).jsonObject
        require(outer.text("type") == "data") { outer.optionalText("message") ?: "Relay connection ended" }
        return wireJson.parseToJsonElement(outer.text("payload")).jsonObject
    }
    private suspend fun sendInner(inner: JsonObject) {
        (socket ?: error("Not connected")).send(obj("type" to JsonPrimitive("data"), "payload" to JsonPrimitive(inner.toString())).toString())
    }
    private suspend fun sendEncrypted(message: JsonObject, feed: TerminalFeed? = null, input: Boolean = false, sizing: Boolean = false) {
        val generation = lifetime.generation
        val session = crypto ?: error("Not authenticated")
        val remote = socket ?: error("Not connected")
        sending.withLock {
            currentCoroutineContext().ensureActive()
            if (!lifetime.accepts(generation) || crypto !== session || socket !== remote ||
                (feed != null && (mutable.value.terminal !== feed || !feed.active)) ||
                (input && (!mutable.value.connected || !mutable.value.terminalSynced)) ||
                (sizing && (!mutable.value.connected || !mutable.value.terminalSynced || feed?.synced != true || feed?.sizeLeaseLost == true))) {
                throw CancellationException("Encrypted send is no longer current")
            }
            commitEncrypted(message, session, remote)
        }
    }
    // Encryption consumes a session sequence. Once committed, cancellation may not drop its envelope.
    // Both encryption and send use the captured connection; any failure retires that connection.
    private suspend fun commitEncrypted(message: JsonObject, session: SecureSession, remote: RemoteSocket) =
        withContext(NonCancellable) {
            try {
                val envelope = withContext(worker) { session.encrypt(message.toString()) }
                remote.send(obj("type" to JsonPrimitive("data"), "payload" to JsonPrimitive(envelope.toString())).toString())
            } catch (e: Throwable) {
                remote.close()
                if (crypto === session && socket === remote) crypto = null
                throw e
            }
        }
    @OptIn(kotlinx.serialization.ExperimentalSerializationApi::class)
    private suspend fun acknowledgeParsed(
        feed: TerminalFeed, frame: TerminalFrame, generation: Long, session: SecureSession, remote: RemoteSocket,
    ) = sending.withLock {
        currentCoroutineContext().ensureActive()
        if (!lifetime.accepts(generation) || crypto !== session || socket !== remote ||
            mutable.value.terminal !== feed || !feed.matches(feed.pane.id, frame.subscriptionId)) return@withLock
        val message = obj("type" to JsonPrimitive("output_ack"), "pane_id" to JsonPrimitive(feed.pane.id),
            "subscription_id" to JsonPrimitive(frame.subscriptionId), "sequence" to JsonUnquotedLiteral(frame.sequence.toString()))
        commitEncrypted(message, session, remote)
    }
    private suspend fun request(
        op: String, vararg fields: Pair<String, JsonElement>, subscribing: TerminalFeed? = null, input: TerminalFeed? = null,
        sizing: TerminalFeed? = null, target: TerminalFeed? = null,
    ): JsonElement {
        check(mutable.value.connected) { "Connect to a computer first" }
        val generation = lifetime.generation
        val id = nextRequest++
        val result = CompletableDeferred<JsonElement>()
        requests[id] = result
        if (subscribing != null) pendingSubscriptions[id] = subscribing
        try {
            sendEncrypted(obj("type" to JsonPrimitive("request"), "id" to JsonPrimitive(id), "op" to JsonPrimitive(op), *fields),
                feed = subscribing ?: input ?: sizing ?: target, input = input != null, sizing = sizing != null)
            val response = withTimeout(requestTimeout) { result.await() }
            if (!lifetime.accepts(generation)) throw CancellationException("Connection changed")
            return response
        } catch (e: TimeoutCancellationException) {
            throw IllegalStateException("$op timed out; the remote operation may still complete. Refresh to reconcile its outcome.", e)
        } finally {
            if (requests.remove(id) != null && lifetime.accepts(generation)) retiredRequests.add(id)
            pendingSubscriptions.remove(id)
        }
    }
    private fun reconcileAgent(pane: Pane): Pane {
        val previous = agentReadings[pane.id]
        val revision = pane.agentRevision
        if (revision != null && (previous == null || revision >= previous.revision)) {
            agentReadings[pane.id] = AgentReading(revision, pane.agentName, pane.agentStatus)
            return pane
        }
        return previous?.apply(pane) ?: pane
    }
    fun refresh() = action {
        if (!refreshing.tryLock()) return@action
        try {
            val terminalIntent = selectionIntent
            val result = request("list").jsonObject
            val workspaces = result.getValue("workspaces").jsonArray.map { value ->
                val workspace = value.jsonObject
                Workspace(workspace.text("id"), workspace.text("name"), workspace.getValue("terminals").jsonArray.map { terminal ->
                    val pane = terminal.jsonObject
                    val agent = pane["agent"]?.takeUnless { it is JsonNull }?.jsonObject
                    reconcileAgent(Pane(pane.getValue("pane_id").jsonPrimitive.long, pane.text("title"), pane.optionalText("cwd"),
                        pane.getValue("cols").jsonPrimitive.int, pane.getValue("rows").jsonPrimitive.int,
                        pane.optionalText("tab_id"), pane.optionalText("group"), pane.optionalText("git_branch"),
                        agent?.optionalText("status"), agent?.optionalText("name"), pane.optionalText("group_id"),
                        pane.optionalText("agent_revision")?.toULong()))
                })
            }
            mutable.value = mutable.value.copy(workspaces = workspaces)
            mutable.value.terminalSelection?.let { (workspace, pane) ->
                val listed = workspaces.firstOrNull { it.id == workspace }?.panes?.firstOrNull { it.id == pane.id }
                if (listed != null) {
                    mutable.value.terminal?.takeIf { it.workspace == workspace && it.pane.id == pane.id }?.updatePane(listed)
                    mutable.value = mutable.value.copy(terminalSelection = workspace to listed)
                } else if (terminalIntent == selectionIntent) leaveTerminal()
            }
        } finally { refreshing.unlock() }
    }
    fun createWorkspace(name: String) = action {
        request("create_workspace", "name" to (name.takeIf { it.isNotBlank() }?.let(::JsonPrimitive) ?: JsonNull)); refresh()
    }
    fun createTerminal(workspace: String) = action {
        request("create_terminal", "workspace_id" to JsonPrimitive(workspace), "cwd" to JsonNull); refresh()
    }
    fun closeTerminal(workspace: String, pane: Pane) = action {
        request("close_terminal", "workspace_id" to JsonPrimitive(workspace), "pane_id" to JsonPrimitive(pane.id))
        if (mutable.value.terminalSelection?.let { it.first == workspace && it.second.id == pane.id } == true) leaveTerminal()
        refresh()
    }
    private fun invalidateTerminalIntent(): Long {
        selectionIntent++
        selectionAction?.cancel()
        selectionAction = null
        return selectionIntent
    }
    private fun clearTerminalSelection() {
        failedTerminal = null
        invalidateTerminalIntent()
        mutable.value.terminal?.cancel()
        mutable.value = mutable.value.copy(terminal = null, terminalSelection = null, terminalSynced = false, terminalError = null)
    }
    fun selectTerminal(workspace: String, pane: Pane): Job? {
        if (!lifetime.active) return null
        failedTerminal = null
        val intent = invalidateTerminalIntent()
        mutable.value.terminal?.cancel()
        mutable.value = mutable.value.copy(terminal = null, terminalSelection = workspace to pane, terminalSynced = false, terminalError = null)
        if (!mutable.value.connected) return null
        val generation = lifetime.generation
        val session = crypto ?: return null
        val remote = socket ?: return null
        fun isCurrent() = selectionIntent == intent && lifetime.accepts(generation) &&
            crypto === session && socket === remote && mutable.value.connected
        val job = action {
            try { switching.withLock {
                if (!isCurrent()) return@withLock
                subscribedPane?.let { previous ->
                    request("unsubscribe", "pane_id" to JsonPrimitive(previous))
                    currentCoroutineContext().ensureActive()
                    if (!isCurrent()) return@withLock
                    subscribedPane = null
                }
                if (!isCurrent()) return@withLock
                val currentPane = mutable.value.terminalSelection?.second ?: pane
                val feed = TerminalFeed(workspace, currentPane, onSynced = { syncedFeed ->
                    if (isCurrent() && mutable.value.terminal === syncedFeed && syncedFeed.active) {
                        mutable.value = mutable.value.copy(terminalSynced = true)
                    }
                }, onResync = { resyncFeed ->
                    action {
                        if (isCurrent() && mutable.value.terminal === resyncFeed && resyncFeed.active) {
                            try {
                                request("resync", "workspace_id" to JsonPrimitive(workspace),
                                    "pane_id" to JsonPrimitive(pane.id),
                                    "subscription_id" to JsonPrimitive(resyncFeed.subscriptionId!!), target = resyncFeed)
                            } catch (e: Exception) {
                                if (e is CancellationException) throw e
                                if (isCurrent() && mutable.value.terminal === resyncFeed) {
                                    retainTerminalFailure(e.message ?: "Terminal resynchronization failed")
                                }
                            }
                        }
                    }
                }, onHistory = { historyFeed, cursor ->
                    check(isCurrent() && mutable.value.terminal === historyFeed && historyFeed.active)
                    HistoryPage.fromJson(request("history", "workspace_id" to JsonPrimitive(workspace),
                        "pane_id" to JsonPrimitive(pane.id), "subscription_id" to JsonPrimitive(historyFeed.subscriptionId!!),
                        "cursor" to (cursor?.let(::JsonPrimitive) ?: JsonNull), target = historyFeed).jsonObject)
                }) { parsedFeed, frame ->
                    acknowledgeParsed(parsedFeed, frame, generation, session, remote)
                }
                subscribedPane = pane.id
                mutable.value = mutable.value.copy(terminal = feed, terminalSynced = false)
                try {
                    // A cold renderer may still be loading its bundled fonts.
                    // Subscribe at a valid grid, then apply its measured size.
                    val size = phoneGrid ?: (currentPane.cols.coerceIn(20, 1000) to currentPane.rows.coerceIn(5, 1000))
                    request("subscribe", "workspace_id" to JsonPrimitive(workspace), "pane_id" to JsonPrimitive(pane.id),
                        "cols" to JsonPrimitive(size.first),
                        "rows" to JsonPrimitive(size.second), subscribing = feed)
                    currentCoroutineContext().ensureActive()
                    if (!isCurrent()) feed.cancel()
                } catch (e: Exception) {
                    feed.cancel()
                    throw e
                }
            } } catch (e: Exception) {
                if (e is CancellationException) throw e
                if (isCurrent()) retainTerminalFailure(e.message ?: "Terminal subscription failed")
            }
        }
        selectionAction = job
        return job
    }
    fun retryTerminal(): Job? {
        val (workspace, pane) = mutable.value.terminalSelection ?: return null
        return selectTerminal(workspace, pane)
    }
    fun leaveTerminal() {
        clearTerminalSelection()
        unsubscribeTerminal(selectionIntent)
    }
    private fun unsubscribeTerminal(intent: Long) {
        selectionAction = action {
            switching.withLock {
                if (intent != selectionIntent || !mutable.value.connected) return@withLock
                val previous = subscribedPane ?: return@withLock
                request("unsubscribe", "pane_id" to JsonPrimitive(previous))
                currentCoroutineContext().ensureActive()
                if (intent == selectionIntent) subscribedPane = null
            }
        }
    }
    private var phoneGrid: Pair<Int, Int>? = null
    fun measuredPhoneGrid(cols: Int, rows: Int) {
        if (cols !in 20..1000 || rows !in 5..1000) return
        phoneGrid = cols to rows
        mutable.value.terminal?.let { resizeTerminal(it, cols, rows) }
    }
    fun resizeTerminal(feed: TerminalFeed, cols: Int, rows: Int): Job? {
        if (mutable.value.terminal !== feed || !feed.active || !feed.synced ||
            !mutable.value.connected || !mutable.value.terminalSynced || feed.sizeLeaseLost) return null
        val subscriptionId = feed.subscriptionId ?: return null
        return action {
            if (mutable.value.terminal !== feed || !feed.active || !feed.synced || feed.sizeLeaseLost) return@action
            require(cols in 20..1000 && rows in 5..1000) { "Terminal size must be 20..1000 columns and 5..1000 rows" }
            try {
                request("terminal_size", "workspace_id" to JsonPrimitive(feed.workspace),
                    "pane_id" to JsonPrimitive(feed.pane.id), "subscription_id" to JsonPrimitive(subscriptionId),
                    "cols" to JsonPrimitive(cols), "rows" to JsonPrimitive(rows), sizing = feed)
            } catch (error: Exception) {
                if (error is CancellationException) throw error
                if (mutable.value.terminal === feed && feed.active) fail(error)
            }
        }
    }
    fun input(text: String) = action {
        if (!mutable.value.connected || !mutable.value.terminalSynced) return@action
        val feed = mutable.value.terminal ?: return@action
        request("input", "workspace_id" to JsonPrimitive(feed.workspace), "pane_id" to JsonPrimitive(feed.pane.id),
            "data" to JsonPrimitive(platform.encodeInput(text)), input = feed)
    }
    fun forget(computer: Computer) = action {
        ensureLoaded()
        if (mutable.value.computers.singleOrNull()?.hostId != computer.hostId) return@action
        automaticConnection = false
        val unpairing = currentCoroutineContext()[Job]
        restoring?.cancel()
        actions.toList().filterNot { it === unpairing }.forEach { it.cancel() }
        clearTerminalSelection()
        disconnectAndJoin()
        storage.withLock {
            currentCoroutineContext().ensureActive()
            withContext(NonCancellable) {
                platform.saveComputers(emptyList())
                lifetime.invalidate()
                mutable.value = RemoteState()
            }
        }
    }
    private suspend fun disconnectAndJoin() { val job = connection; job?.cancel(); socket?.close(); job?.join(); connection = null }
    private fun stopConnection() {
        lifetime.invalidate()
        invalidateTerminalIntent()
        mutable.value.terminal?.cancel()
        actions.toList().forEach { it.cancel() }
        connection?.cancel()
        socket?.close()
        mutable.value = mutable.value.copy(connected = false, terminal = null, terminalSynced = false, reconnecting = false)
    }
    fun disconnect() {
        automaticConnection = false
        clearTerminalSelection()
        stopConnection()
    }
    private fun retainTerminalFailure(message: String) {
        val intent = invalidateTerminalIntent()
        failedTerminal = mutable.value.terminal
        mutable.value.terminal?.cancel()
        mutable.value = mutable.value.copy(terminal = null, terminalSynced = false, terminalError = message)
        unsubscribeTerminal(intent)
    }
    fun terminalFailed(feed: TerminalFeed, message: String) {
        if (mutable.value.terminal !== feed || !feed.active) return
        retainTerminalFailure(message)
    }
    fun clearError() { mutable.value = mutable.value.copy(error = null) }
}
