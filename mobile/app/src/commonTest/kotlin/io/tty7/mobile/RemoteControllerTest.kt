package io.tty7.mobile

import kotlinx.coroutines.*
import kotlinx.coroutines.channels.Channel
import kotlinx.coroutines.test.*
import kotlinx.serialization.json.*
import kotlin.test.*

@OptIn(ExperimentalCoroutinesApi::class)
class RemoteControllerTest {
    private val approved = Computer("host", "Computer", DEFAULT_RELAY, "pin", "device-old", "old-secret")
    private val pending = approved.copy(credentialId = "pair:bootstrap", secret = "bootstrap-secret", expiresAt = 300)
    private val replacement = approved.copy(credentialId = "device-new", secret = "new-secret")
    private val pane = Pane(1, "shell", null, 80, 24, tabId = "tab", group = "Project", gitBranch = "main",
        agentStatus = "idle", agentName = "Pi", groupId = "pinned:project", agentRevision = 0uL)

    @Test fun singleComputerPolicyRejectsReplacementUntilUnpaired() {
        assertEquals(approved, retainedComputer(listOf(approved, pending)))
        assertEquals(pending, retainedComputer(listOf(pending)))
        assertNull(retainedComputer(emptyList()))
        assertEquals(pending, updatedComputer(null, pending, newPair = true))
        assertEquals(replacement, updatedComputer(pending, replacement))
        assertFailsWith<IllegalStateException> { updatedComputer(approved, pending, newPair = true) }
        assertFailsWith<IllegalStateException> { updatedComputer(approved, pending) }
        assertFailsWith<IllegalStateException> { updatedComputer(approved, replacement.copy(hostId = "other")) }
        assertFailsWith<IllegalStateException> { updatedComputer(null, approved) }
    }

    @Test fun sameHostDeepLinkCannotReplaceWorkingCredential() = runTest {
        val platform = ControlledPlatform(listOf(approved), pending)
        val controller = authenticatedController(platform)
        val feed = openTerminal(controller, platform.socket)
        controller.pair("pairing-link")
        runCurrent()
        assertEquals(listOf(approved), platform.disk)
        assertEquals(listOf(approved), controller.state.value.computers)
        assertEquals(UNPAIR_BEFORE_PAIRING, controller.state.value.error)
        assertEquals(listOf(approved), platform.connections)
        assertSame(feed, controller.state.value.terminal)
        assertTrue(controller.state.value.connected)
        controller.setForeground(false)
    }

    @Test fun launchRestoresMostRecentApprovedComputerNotPendingQr() = runTest {
        val recent = approved.copy(hostId = "recent", hostName = "Recent")
        val platform = ControlledPlatform(listOf(approved, recent, pending), pending)
        platform.socket.authenticate("Recent")
        val controller = controller(platform)
        controller.setForeground(true)
        runCurrent()
        platform.socket.response(1, listing())
        runCurrent()
        assertEquals(listOf(recent), platform.connections)
        assertEquals(recent, controller.state.value.computer)
        assertTrue(controller.state.value.connected)
        assertEquals(listOf(recent), platform.disk)
        controller.setForeground(false)
    }

    @Test fun differentComputerCannotConnectOrPairBeforeUnpairing() = runTest {
        val other = pending.copy(hostId = "other", hostName = "Other")
        val platform = ControlledPlatform(listOf(approved), other)
        val controller = authenticatedController(platform)
        controller.connect(other)
        controller.pair("other-link")
        runCurrent()
        assertEquals(UNPAIR_BEFORE_PAIRING, controller.state.value.error)
        assertEquals(listOf(approved), platform.disk)
        assertEquals(listOf(approved), platform.connections)
        assertEquals(approved, controller.state.value.computer)
        assertTrue(controller.state.value.connected)
        controller.setForeground(false)
    }

    @Test fun concurrentPairingLinksStoreOnlyOnePendingComputer() = runTest {
        val platform = ControlledPlatform(emptyList(), pending)
        val controller = controller(platform)
        controller.setForeground(true)
        controller.pair("first-link")
        controller.pair("second-link")
        runCurrent()
        assertEquals(listOf(pending), platform.disk)
        assertEquals(listOf(pending), controller.state.value.computers)
        assertEquals(listOf(pending), platform.connections)
        assertEquals(UNPAIR_BEFORE_PAIRING, controller.state.value.error)
        controller.setForeground(false)
    }

    @Test fun unpairClosesSocketAndAllowsNewPairWithoutAutomaticReconnection() = runTest {
        val other = pending.copy(hostId = "other", hostName = "Other")
        val platform = ControlledPlatform(listOf(approved), other)
        val controller = authenticatedController(platform)
        val feed = openTerminal(controller, platform.socket)
        controller.forget(approved)
        runCurrent()
        assertTrue(platform.socket.closed)
        assertFalse(feed.active)
        assertTrue(platform.disk.isEmpty())
        assertNull(controller.state.value.computer)
        assertNull(controller.state.value.terminalSelection)
        assertEquals("Disconnected", controller.state.value.status)
        controller.networkChanged(true)
        controller.setForeground(false)
        controller.setForeground(true)
        runCurrent()
        assertEquals(listOf(approved), platform.connections)
        platform.nextSocket().handshake()
        controller.pair("other-link")
        runCurrent()
        assertEquals(listOf(other), platform.disk)
        assertEquals(other, controller.state.value.computer)
        assertEquals(listOf(approved, other), platform.connections)
        controller.setForeground(false)
    }

    @Test fun stoppingDuringLoadPreventsSocketUntilForegroundReturns() = runTest {
        val platform = ControlledPlatform(listOf(approved), pending)
        val release = CompletableDeferred<Unit>()
        platform.loadRelease = release
        val controller = controller(platform)
        controller.setForeground(true)
        runCurrent()
        controller.setForeground(false)
        release.complete(Unit)
        runCurrent()
        assertTrue(platform.connections.isEmpty())
        platform.socket.authenticate()
        controller.setForeground(true)
        runCurrent()
        assertEquals(listOf(approved), platform.connections)
        assertTrue(controller.state.value.connected)
        controller.setForeground(false)
    }

    @Test fun approvalCommittedWhileStoppedRestoresNewCredentialOnReturn() = runTest {
        val platform = ControlledPlatform(emptyList(), pending)
        val release = CompletableDeferred<Unit>()
        platform.afterCommit = { entries -> if (entries.any { it.credentialId == replacement.credentialId }) release.await() }
        platform.socket.handshake()
        platform.socket.encrypted(obj("type" to JsonPrimitive("pair_pending"), "request_id" to JsonPrimitive("request"), "code" to JsonPrimitive("12345678")))
        platform.socket.encrypted(obj("type" to JsonPrimitive("paired"), "credential_id" to JsonPrimitive(replacement.credentialId), "secret" to JsonPrimitive(replacement.secret)))
        val controller = controller(platform)
        controller.setForeground(true)
        controller.pair("pairing-link")
        runCurrent()
        assertEquals(listOf(replacement), platform.disk)
        controller.setForeground(false)
        release.complete(Unit)
        runCurrent()
        assertEquals(1, platform.connections.size)
        assertEquals(replacement, controller.state.value.computer)
        platform.nextSocket().authenticate()
        controller.setForeground(true)
        runCurrent()
        assertEquals(listOf(pending, replacement), platform.connections)
        assertTrue(controller.state.value.connected)
        controller.setForeground(false)
    }

    @Test fun backgroundReturnRestoresTerminalWithFreshSnapshotSubscription() = runTest {
        val platform = ControlledPlatform(listOf(approved), pending)
        val controller = authenticatedController(platform)
        val original = openTerminal(controller, platform.socket)
        controller.setForeground(false)
        runCurrent()
        assertFalse(controller.state.value.connected)
        assertEquals("workspace" to pane, controller.state.value.terminalSelection)
        assertFalse(controller.state.value.terminalSynced)
        val resumed = platform.nextSocket().apply { authenticate() }
        controller.setForeground(true)
        runCurrent()
        resumed.response(3, listing())
        resumed.subscribed(4, "restored-subscription")
        resumed.output("restored", snapshot = true)
        
        runCurrent()
        val feed = assertNotNull(controller.state.value.terminal)
        assertNotSame(original, feed)
        assertEquals(pane.id, feed.pane.id)
        val snapshot = feed.frames.tryReceive().getOrThrow()
        assertEquals("restored", snapshot.lines.first().ansi)
        assertEquals("restored-subscription", snapshot.subscriptionId)
        assertEquals(1uL, snapshot.sequence)
        assertFalse(controller.state.value.terminalSynced)
        
        runCurrent()
        assertFalse(controller.state.value.terminalSynced)
        feed.acknowledgeParsed(snapshot)
        assertTrue(controller.state.value.terminalSynced)
        assertTrue(controller.state.value.connected)
        controller.setForeground(false)
    }

    @Test fun handshakeTimeoutIsVisibleAndAutomaticallyRetriesWithoutQr() = runTest {
        val platform = ControlledPlatform(listOf(approved), pending)
        val controller = controller(platform)
        controller.setForeground(true)
        runCurrent()
        advanceTimeBy(1_001)
        runCurrent()
        assertTrue(controller.state.value.error.orEmpty().contains("Connection timed out"))
        assertTrue(platform.socket.closed)
        assertEquals(approved, controller.state.value.computer)
        assertTrue(controller.state.value.reconnecting)
        platform.nextSocket().authenticate()
        advanceTimeBy(1_001)
        runCurrent()
        assertEquals(2, platform.connections.size)
        assertTrue(controller.state.value.connected)
        controller.setForeground(false)
    }

    @Test fun explicitDisconnectSuppressesReconnectUntilUserConnects() = runTest {
        val platform = ControlledPlatform(listOf(approved), pending)
        val controller = authenticatedController(platform)
        openTerminal(controller, platform.socket)
        controller.disconnect()
        runCurrent()
        assertNull(controller.state.value.terminalSelection)
        assertFalse(controller.state.value.terminalSynced)
        advanceTimeBy(60_000)
        controller.setForeground(false)
        controller.setForeground(true)
        runCurrent()
        assertEquals(1, platform.connections.size)
        assertFalse(controller.state.value.reconnecting)
        platform.nextSocket().authenticate()
        controller.connect(approved)
        runCurrent()
        assertTrue(controller.state.value.connected)
        controller.setForeground(false)
    }

    @Test fun lateResponseDoesNotCloseTerminalButUnknownResponseClosesSession() = runTest {
        val platform = ControlledPlatform(listOf(approved), pending)
        val controller = authenticatedController(platform)
        val feed = openTerminal(controller, platform.socket)
        controller.input("x")
        runCurrent()
        advanceTimeBy(1_001)
        runCurrent()
        assertTrue(controller.state.value.error.orEmpty().contains("remote operation may still complete"))
        platform.socket.response(3, JsonNull)
        runCurrent()
        assertTrue(controller.state.value.connected)
        assertSame(feed, controller.state.value.terminal)
        platform.socket.response(999, JsonNull)
        runCurrent()
        assertFalse(controller.state.value.connected)
        assertEquals("Unexpected response ID", controller.state.value.error)
        controller.setForeground(false)
    }

    @Test fun rendererFailureRetainsPageAndRetryWaitsForFreshMatchingCompletion() = runTest {
        val platform = ControlledPlatform(listOf(approved), pending)
        val controller = authenticatedController(platform)
        val feed = openTerminal(controller, platform.socket)
        repeat(4) { platform.socket.output("frame-$it") }
        runCurrent()
        controller.terminalFailed(feed, "Terminal renderer timed out")
        runCurrent()
        platform.socket.response(3, JsonNull)
        runCurrent()
        assertTrue(controller.state.value.connected)
        assertEquals(approved, controller.state.value.computer)
        assertEquals("workspace", controller.state.value.workspaces.single().id)
        assertNull(controller.state.value.terminal)
        assertEquals("workspace" to pane, controller.state.value.terminalSelection)
        assertFalse(controller.state.value.terminalSynced)
        assertEquals("Terminal renderer timed out", controller.state.value.terminalError)
        assertNull(controller.state.value.error)
        assertTrue(feed.frames.tryReceive().isFailure)
        controller.input("blocked")
        runCurrent()
        assertTrue(platform.socket.encryptedMessages.none { it.optionalText("op") == "input" })
        assertEquals(1, platform.socket.encryptedMessages.count { it.optionalText("op") == "subscribe" })
        controller.retryTerminal()
        assertNull(controller.state.value.terminalError)
        runCurrent()
        val subscription = "33809975-bdf0-4922-98b5-eb2b4b447261"
        platform.socket.subscribed(4, subscription)
        platform.socket.output("after-retry", snapshot = true)
        
        platform.socket.encrypted(obj("type" to JsonPrimitive("terminal_error"), "pane_id" to JsonPrimitive(pane.id),
            "subscription_id" to JsonPrimitive("subscription-1"), "message" to JsonPrimitive("Late observer error")))
        runCurrent()
        val reopened = assertNotNull(controller.state.value.terminal)
        assertNotSame(feed, reopened)
        controller.terminalFailed(feed, "Late renderer error")
        assertSame(reopened, controller.state.value.terminal)
        assertNull(controller.state.value.terminalError)
        assertFalse(controller.state.value.terminalSynced)
        val snapshot = reopened.frames.tryReceive().getOrThrow()
        assertEquals(subscription, snapshot.subscriptionId)
        assertNotEquals(feed.subscriptionId, reopened.subscriptionId)
        assertEquals("after-retry", snapshot.lines.first().ansi)
        reopened.acknowledgeParsed(snapshot)
        runCurrent()
        assertTrue(controller.state.value.terminalSynced)
        assertNull(controller.state.value.terminalError)
        controller.setForeground(false)
    }

    @Test fun agentAndGroupedMetadataRefreshWhileTerminalRemainsOpen() = runTest {
        val platform = ControlledPlatform(listOf(approved), pending)
        val controller = authenticatedController(platform)
        val feed = openTerminal(controller, platform.socket)
        advanceTimeBy(2_000)
        runCurrent()
        platform.socket.response(3, listing("working", "Project", "feature"))
        runCurrent()
        val listed = controller.state.value.workspaces.single().panes.single()
        assertEquals("Project", listed.group)
        assertEquals("pinned:project", listed.groupId)
        assertEquals("feature", listed.gitBranch)
        assertEquals("working", listed.agentStatus)
        assertEquals("Pi", listed.agentName)
        assertSame(feed, controller.state.value.terminal)
        platform.socket.output("live-agent-output")
        runCurrent()
        assertEquals("live-agent-output", feed.frames.tryReceive().getOrThrow().lines.first().ansi)
        advanceTimeBy(2_000)
        runCurrent()
        platform.socket.response(4, listing("done", "Renamed", "main"))
        runCurrent()
        assertEquals("done", controller.state.value.workspaces.single().panes.single().agentStatus)
        assertEquals("Renamed", controller.state.value.workspaces.single().panes.single().group)
        assertSame(feed, controller.state.value.terminal)
        controller.setForeground(false)
    }

    @Test fun liveAgentStateWinsOverOlderListAndReplayedCompletion() = runTest {
        val platform = ControlledPlatform(listOf(approved), pending)
        val controller = authenticatedController(platform)
        val feed = openTerminal(controller, platform.socket)
        controller.refresh()
        runCurrent()
        platform.socket.agent("working", revision = 2)
        runCurrent()
        platform.socket.response(3, listing("done", revision = 1))
        platform.socket.agent("done", revision = 1)
        runCurrent()
        assertTrue(controller.state.value.connected)
        assertEquals("working", controller.state.value.workspaces.single().panes.single().agentStatus)
        assertEquals("working", controller.state.value.terminalSelection?.second?.agentStatus)
        assertEquals("working", feed.pane.agentStatus)
        assertSame(feed, controller.state.value.terminal)
        for ((revision, status) in listOf(3L to "waiting", 4L to "working", 5L to "done")) {
            platform.socket.agent(status, revision)
            runCurrent()
            assertEquals(status, controller.state.value.workspaces.single().panes.single().agentStatus)
            assertEquals(status, controller.state.value.terminalSelection?.second?.agentStatus)
            assertEquals(status, feed.pane.agentStatus)
        }
        platform.socket.agent(null, revision = 6)
        runCurrent()
        assertNull(controller.state.value.workspaces.single().panes.single().agentStatus)
        assertNull(feed.pane.agentName)
        controller.setForeground(false)
    }

    @Test fun reconnectRejectsOldSubscriptionCompletionButAcceptsNewDaemonRevision() = runTest {
        val platform = ControlledPlatform(listOf(approved), pending)
        val controller = authenticatedController(platform)
        val original = openTerminal(controller, platform.socket)
        platform.socket.agent("done", revision = 99)
        runCurrent()
        controller.setForeground(false)
        runCurrent()
        val resumed = platform.nextSocket().apply { authenticate() }
        controller.setForeground(true)
        runCurrent()
        resumed.subscribed(4, "fresh-agent-subscription")
        resumed.agent("working", revision = 2)
        resumed.response(3, listing("done", revision = 1))
        resumed.agent("done", revision = 100, subscription = "subscription-1")
        renderEmpty(controller, resumed)
        runCurrent()
        assertTrue(controller.state.value.connected)
        val current = assertNotNull(controller.state.value.terminal)
        assertNotSame(original, current)
        assertEquals("working", controller.state.value.workspaces.single().panes.single().agentStatus)
        assertEquals("working", current.pane.agentStatus)
        assertEquals("working", controller.state.value.terminalSelection?.second?.agentStatus)
        controller.setForeground(false)
    }

    @Test fun desktopClosureReturnsToListWithoutDroppingConnection() = runTest {
        val platform = ControlledPlatform(listOf(approved), pending)
        val controller = authenticatedController(platform)
        openTerminal(controller, platform.socket)
        advanceTimeBy(2_000)
        runCurrent()
        platform.socket.response(3, obj("workspaces" to JsonArray(emptyList())))
        runCurrent()
        platform.socket.response(4, JsonNull)
        runCurrent()
        assertNull(controller.state.value.terminal)
        assertNull(controller.state.value.terminalSelection)
        assertTrue(controller.state.value.connected)
        assertTrue(controller.state.value.workspaces.isEmpty())
        controller.setForeground(false)
    }

    @Test fun unsupportedChallengeCannotAuthenticateOrReplaceSavedCredential() = runTest {
        for (protocol in listOf(null, JsonPrimitive(1), JsonPrimitive(2), JsonPrimitive("3"))) {
            val platform = ControlledPlatform(listOf(approved), pending)
            platform.socket.handshake(protocol)
            val controller = controller(platform)
            controller.setForeground(true)
            runCurrent()
            assertFalse(controller.state.value.connected)
            assertEquals("Unsupported mobile protocol. Update phone and computer.", controller.state.value.error)
            assertEquals(listOf(approved), platform.disk)
            assertTrue(platform.socket.encryptedMessages.isEmpty())
            controller.setForeground(false)
            runCurrent()
        }
    }

    @Test fun slowRendererLeavesMetadataResponsiveAndBlocksInputUntilInitialFramesAreParsed() = runTest {
        val platform = ControlledPlatform(listOf(approved), pending)
        val controller = authenticatedController(platform)
        controller.selectTerminal("workspace", pane)
        runCurrent()
        platform.socket.subscribed(2)
        repeat(4) { platform.socket.output("credit-$it", snapshot = it == 0) }
        
        runCurrent()
        val feed = assertNotNull(controller.state.value.terminal)
        val input = assertNotNull(controller.input("still-responsive"))
        runCurrent()
        assertTrue(input.isCompleted)
        assertTrue(platform.socket.encryptedMessages.none { it.optionalText("op") == "input" })
        assertFalse(controller.state.value.terminalSynced)
        advanceTimeBy(2_000)
        runCurrent()
        platform.socket.response(3, listing("working", "Fresh", "credit"))
        runCurrent()
        assertEquals("Fresh", controller.state.value.workspaces.single().panes.single().group)
        assertSame(feed, controller.state.value.terminal)
        assertNull(controller.state.value.error)
        assertTrue(platform.socket.acknowledgments.isEmpty())
        val first = feed.frames.tryReceive().getOrThrow()
        assertEquals("credit-0", first.lines.first().ansi)
        assertTrue(platform.socket.acknowledgments.isEmpty())
        feed.acknowledgeParsed(first)
        val ack = platform.socket.acknowledgments.single()
        assertEquals(pane.id, ack.getValue("pane_id").jsonPrimitive.long)
        assertEquals(first.subscriptionId, ack.text("subscription_id"))
        assertEquals(1, ack.getValue("sequence").jsonPrimitive.int)
        assertFalse(ack.getValue("sequence").jsonPrimitive.isString)
        assertNull(ack["id"])
        repeat(3) { feed.acknowledgeParsed(feed.frames.tryReceive().getOrThrow()) }
        assertTrue(controller.state.value.terminalSynced)
        controller.input("now-responsive")
        runCurrent()
        platform.socket.response(4, JsonNull)
        runCurrent()
        assertEquals("now-responsive", platform.socket.encryptedMessages.single { it.optionalText("op") == "input" }.text("data"))
        assertTrue(controller.state.value.connected)
        controller.setForeground(false)
    }

    @Test fun staleSamePaneOutputExitAndErrorCannotAffectReopenedSubscription() = runTest {
        val platform = ControlledPlatform(listOf(approved), pending)
        val controller = authenticatedController(platform)
        val original = openTerminal(controller, platform.socket)
        platform.socket.output("old-frame", snapshot = true)
        runCurrent()
        val oldFrame = original.frames.tryReceive().getOrThrow()
        controller.selectTerminal("workspace", pane)
        runCurrent()
        platform.socket.response(3, JsonNull)
        runCurrent()
        platform.socket.output("before-new-subscription", snapshot = true)
        platform.socket.subscribed(4, "new-subscription")
        platform.socket.output("stale-reset", snapshot = true, subscriptionId = oldFrame.subscriptionId, sequence = 3)
        platform.socket.encrypted(obj("type" to JsonPrimitive("exited"), "pane_id" to JsonPrimitive(pane.id),
            "subscription_id" to JsonPrimitive(oldFrame.subscriptionId), "code" to JsonPrimitive(1)))
        platform.socket.encrypted(obj("type" to JsonPrimitive("terminal_error"), "pane_id" to JsonPrimitive(pane.id),
            "subscription_id" to JsonPrimitive(oldFrame.subscriptionId), "message" to JsonPrimitive("Old observer failed")))
        platform.socket.output("new-frame", snapshot = true, sequence = 1)
        runCurrent()
        val reopened = assertNotNull(controller.state.value.terminal)
        assertNotSame(original, reopened)
        val currentFrame = reopened.frames.tryReceive().getOrThrow()
        assertEquals("new-frame", currentFrame.lines.first().ansi)
        assertEquals("new-subscription", currentFrame.subscriptionId)
        assertTrue(reopened.frames.tryReceive().isFailure)
        assertTrue(controller.state.value.connected)
        assertEquals("Connected", controller.state.value.status)
        assertNull(controller.state.value.error)
        assertNull(controller.state.value.terminalError)
        original.acknowledgeParsed(oldFrame)
        assertTrue(platform.socket.acknowledgments.isEmpty())
        reopened.acknowledgeParsed(currentFrame)
        assertEquals("new-subscription", platform.socket.acknowledgments.single().text("subscription_id"))
        controller.setForeground(false)
    }

    @Test fun observerFailureRetainsPageAndExplicitRetryRecoversWithoutReauthentication() = runTest {
        val platform = ControlledPlatform(listOf(approved), pending)
        val controller = authenticatedController(platform)
        val feed = openTerminal(controller, platform.socket)
        platform.socket.output("pending-render")
        runCurrent()
        val unparsed = feed.frames.tryReceive().getOrThrow()
        platform.socket.encrypted(obj("type" to JsonPrimitive("terminal_error"), "pane_id" to JsonPrimitive(pane.id),
            "subscription_id" to JsonPrimitive(unparsed.subscriptionId), "message" to JsonPrimitive("Observer disconnected")))
        runCurrent()
        platform.socket.response(3, JsonNull)
        runCurrent()
        assertTrue(controller.state.value.connected)
        assertEquals(approved, controller.state.value.computer)
        assertEquals("workspace", controller.state.value.workspaces.single().id)
        assertEquals("workspace" to pane, controller.state.value.terminalSelection)
        assertNull(controller.state.value.terminal)
        assertFalse(controller.state.value.terminalSynced)
        assertEquals("Observer disconnected", controller.state.value.terminalError)
        assertNull(controller.state.value.error)
        feed.acknowledgeParsed(unparsed)
        assertTrue(platform.socket.acknowledgments.isEmpty())
        controller.retryTerminal()
        runCurrent()
        platform.socket.subscribed(4, "after-observer-error")
        platform.socket.output("reopened", snapshot = true)
        
        runCurrent()
        val reopened = assertNotNull(controller.state.value.terminal)
        assertNotEquals(feed.subscriptionId, reopened.subscriptionId)
        assertNull(controller.state.value.terminalError)
        assertFalse(controller.state.value.terminalSynced)
        val frame = reopened.frames.tryReceive().getOrThrow()
        assertEquals("reopened", frame.lines.first().ansi)
        reopened.acknowledgeParsed(frame)
        assertTrue(controller.state.value.terminalSynced)
        assertTrue(controller.state.value.connected)
        assertEquals(1, platform.connections.size)
        controller.setForeground(false)
    }

    @Test fun rejectedSubscribeKeepsTargetAndReasonUntilExplicitRetry() = runTest {
        val platform = ControlledPlatform(listOf(approved), pending)
        val controller = authenticatedController(platform)
        controller.selectTerminal("workspace", pane)
        runCurrent()
        val rejected = assertNotNull(controller.state.value.terminal)
        platform.socket.encrypted(obj("type" to JsonPrimitive("response"), "id" to JsonPrimitive(2),
            "ok" to JsonPrimitive(false), "error" to JsonPrimitive("Terminal observer could not start")))
        runCurrent()
        platform.socket.response(3, JsonNull)
        runCurrent()
        assertTrue(controller.state.value.connected)
        assertEquals("workspace" to pane, controller.state.value.terminalSelection)
        assertEquals("Terminal observer could not start", controller.state.value.terminalError)
        assertNull(controller.state.value.terminal)
        assertFalse(rejected.active)
        assertNull(controller.state.value.error)
        controller.clearError()
        assertEquals("Terminal observer could not start", controller.state.value.terminalError)
        controller.retryTerminal()
        runCurrent()
        platform.socket.subscribed(4, "retry-after-rejected-subscribe")
        renderEmpty(controller, platform.socket)
        runCurrent()
        controller.terminalFailed(rejected, "Late callback from rejected feed")
        assertNull(controller.state.value.terminalError)
        assertTrue(controller.state.value.terminalSynced)
        assertEquals("retry-after-rejected-subscribe", assertNotNull(controller.state.value.terminal).subscriptionId)
        controller.setForeground(false)
    }

    @Test fun failedTerminalDoesNotAutomaticallyRetryOnForegroundOrNetworkReconnect() = runTest {
        val platform = ControlledPlatform(listOf(approved), pending)
        val controller = authenticatedController(platform)
        val feed = openTerminal(controller, platform.socket)
        controller.terminalFailed(feed, "Renderer unavailable")
        runCurrent()
        platform.socket.response(3, JsonNull)
        runCurrent()
        controller.setForeground(false)
        runCurrent()
        val resumed = platform.nextSocket()
        resumed.authenticate()
        controller.setForeground(true)
        runCurrent()
        resumed.response(4, listing())
        runCurrent()
        assertTrue(controller.state.value.connected)
        assertEquals("workspace" to pane, controller.state.value.terminalSelection)
        assertEquals("Renderer unavailable", controller.state.value.terminalError)
        assertNull(controller.state.value.terminal)
        assertTrue(resumed.encryptedMessages.none { it.optionalText("op") == "subscribe" })
        controller.networkChanged(false)
        runCurrent()
        val rerouted = platform.nextSocket()
        rerouted.authenticate()
        controller.networkChanged(true)
        runCurrent()
        rerouted.response(5, listing())
        runCurrent()
        assertEquals("Renderer unavailable", controller.state.value.terminalError)
        assertTrue(rerouted.encryptedMessages.none { it.optionalText("op") == "subscribe" })
        controller.retryTerminal()
        runCurrent()
        rerouted.subscribed(6, "explicit-retry-after-network-change")
        renderEmpty(controller, rerouted)
        runCurrent()
        assertTrue(controller.state.value.terminalSynced)
        assertNull(controller.state.value.terminalError)
        controller.setForeground(false)
    }

    @Test fun explicitNavigationAndCurrentExitClearRetainedTerminalFailure() = runTest {
        for (navigation in listOf("leave", "disconnect", "forget", "closed", "exited")) {
            val platform = ControlledPlatform(listOf(approved), pending)
            val controller = authenticatedController(platform)
            val feed = openTerminal(controller, platform.socket)
            controller.terminalFailed(feed, "Renderer unavailable")
            runCurrent()
            platform.socket.response(3, JsonNull)
            runCurrent()
            when (navigation) {
                "leave" -> controller.leaveTerminal()
                "disconnect" -> controller.disconnect()
                "forget" -> controller.forget(approved)
                "closed" -> {
                    controller.refresh()
                    runCurrent()
                    platform.socket.response(4, obj("workspaces" to JsonArray(emptyList())))
                }
                "exited" -> {
                    platform.socket.encrypted(obj("type" to JsonPrimitive("exited"), "pane_id" to JsonPrimitive(pane.id),
                        "subscription_id" to JsonPrimitive("stale-generation"), "code" to JsonPrimitive(0)))
                    runCurrent()
                    assertEquals("Renderer unavailable", controller.state.value.terminalError)
                    platform.socket.encrypted(obj("type" to JsonPrimitive("exited"), "pane_id" to JsonPrimitive(pane.id),
                        "subscription_id" to JsonPrimitive(feed.subscriptionId!!), "code" to JsonPrimitive(0)))
                }
            }
            runCurrent()
            assertNull(controller.state.value.terminalSelection, navigation)
            assertNull(controller.state.value.terminalError, navigation)
            assertNull(controller.state.value.terminal, navigation)
            assertFalse(controller.state.value.terminalSynced, navigation)
            assertNull(controller.retryTerminal(), navigation)
            controller.setForeground(false)
            runCurrent()
        }
    }

    @Test fun newSelectionClearsFailureAndRejectsStaleRendererAndObserverErrors() = runTest {
        val platform = ControlledPlatform(listOf(approved), pending)
        val controller = authenticatedController(platform)
        val failed = openTerminal(controller, platform.socket)
        controller.terminalFailed(failed, "Old terminal failed")
        runCurrent()
        platform.socket.response(3, JsonNull)
        runCurrent()
        val other = pane.copy(id = 2, title = "other")
        controller.selectTerminal("workspace", other)
        runCurrent()
        controller.terminalFailed(failed, "Late native failure")
        platform.socket.encrypted(obj("type" to JsonPrimitive("terminal_error"), "pane_id" to JsonPrimitive(pane.id),
            "subscription_id" to JsonPrimitive(failed.subscriptionId!!), "message" to JsonPrimitive("Late observer failure")))
        platform.socket.subscribed(4, "new-target-generation")
        renderEmpty(controller, platform.socket, paneId = other.id)
        runCurrent()
        assertEquals("workspace" to other, controller.state.value.terminalSelection)
        assertEquals(other, assertNotNull(controller.state.value.terminal).pane)
        assertNull(controller.state.value.terminalError)
        assertTrue(controller.state.value.terminalSynced)
        controller.setForeground(false)
    }

    @Test fun observerErrorBeforeSubscribeContinuationRetainsLoadingTargetAndCanRetry() = runTest {
        val platform = ControlledPlatform(listOf(approved), pending)
        val controller = authenticatedController(platform)
        controller.selectTerminal("workspace", pane)
        runCurrent()
        val failed = assertNotNull(controller.state.value.terminal)
        platform.socket.subscribed(2, "failed-initial-replay")
        platform.socket.encrypted(obj("type" to JsonPrimitive("terminal_error"), "pane_id" to JsonPrimitive(pane.id),
            "subscription_id" to JsonPrimitive("failed-initial-replay"), "message" to JsonPrimitive("Initial observer disconnected")))
        runCurrent()
        assertEquals("workspace" to pane, controller.state.value.terminalSelection)
        assertEquals("Initial observer disconnected", controller.state.value.terminalError)
        assertNull(controller.state.value.terminal)
        assertFalse(controller.state.value.terminalSynced)
        assertTrue(controller.state.value.connected)
        platform.socket.response(3, JsonNull)
        runCurrent()
        controller.retryTerminal()
        runCurrent()
        platform.socket.subscribed(4, "fresh-initial-replay")
        
        runCurrent()
        assertFalse(controller.state.value.terminalSynced)
        controller.terminalFailed(failed, "Canceled startup renderer failed")
        assertNull(controller.state.value.terminalError)
        renderEmpty(controller, platform.socket)
        runCurrent()
        assertTrue(controller.state.value.terminalSynced)
        assertEquals("fresh-initial-replay", assertNotNull(controller.state.value.terminal).subscriptionId)
        controller.setForeground(false)
    }

    @Test fun disposedFeedCannotAcknowledgeIntoReconnectedHostSession() = runTest {
        val platform = ControlledPlatform(listOf(approved), pending)
        val controller = authenticatedController(platform)
        val original = openTerminal(controller, platform.socket)
        platform.socket.output("before-background")
        runCurrent()
        val oldFrame = original.frames.tryReceive().getOrThrow()
        controller.setForeground(false)
        runCurrent()
        val resumed = platform.nextSocket().apply { authenticate() }
        controller.setForeground(true)
        runCurrent()
        resumed.response(3, listing())
        resumed.subscribed(4, "fresh-host-session")
        resumed.output("after-background", snapshot = true)
        runCurrent()
        original.acknowledgeParsed(oldFrame)
        assertTrue(platform.socket.acknowledgments.isEmpty())
        assertTrue(resumed.acknowledgments.isEmpty())
        val current = assertNotNull(controller.state.value.terminal)
        current.acknowledgeParsed(current.frames.tryReceive().getOrThrow())
        assertEquals("fresh-host-session", resumed.acknowledgments.single().text("subscription_id"))
        assertTrue(controller.state.value.connected)
        controller.setForeground(false)
    }

    @Test fun invalidParsedNotificationCannotReleaseCredit() = runTest {
        val platform = ControlledPlatform(listOf(approved), pending)
        val controller = authenticatedController(platform)
        val feed = openTerminal(controller, platform.socket)
        platform.socket.output("first")
        platform.socket.output("second")
        runCurrent()
        val first = feed.frames.tryReceive().getOrThrow()
        val second = feed.frames.tryReceive().getOrThrow()
        assertFailsWith<IllegalStateException> { feed.acknowledgeParsed(second) }
        assertFailsWith<IllegalStateException> { feed.acknowledgeParsed(first.copy(subscriptionId = "stale")) }
        assertTrue(platform.socket.acknowledgments.isEmpty())
        feed.acknowledgeParsed(first)
        assertFailsWith<IllegalStateException> { feed.acknowledgeParsed(first) }
        assertEquals(1, platform.socket.acknowledgments.size)
        feed.acknowledgeParsed(second)
        assertEquals(listOf(2, 3), platform.socket.acknowledgments.map { it.getValue("sequence").jsonPrimitive.int })
        assertTrue(controller.state.value.connected)
        controller.setForeground(false)
    }

    @Test fun invalidOrRepeatedOutputSequenceCannotSilentlyCorruptTerminal() = runTest {
        for (sequence in listOf(0L, 2L, 4L)) {
            val platform = ControlledPlatform(listOf(approved), pending)
            val controller = authenticatedController(platform)
            val feed = openTerminal(controller, platform.socket)
            platform.socket.output("first")
            platform.socket.output("invalid", sequence = sequence)
            runCurrent()
            assertFalse(controller.state.value.connected)
            assertEquals("Invalid terminal output sequence", controller.state.value.error)
            assertEquals("first", feed.frames.tryReceive().getOrThrow().lines.first().ansi)
            assertTrue(feed.frames.tryReceive().isFailure)
            assertTrue(platform.socket.acknowledgments.isEmpty())
            controller.setForeground(false)
            runCurrent()
        }
    }

    @Test fun fullStateRevealsImmediatelyAfterApplicationWithoutCompletionMarker() = runTest {
        val platform = ControlledPlatform(listOf(approved), pending)
        val controller = authenticatedController(platform)
        controller.selectTerminal("workspace", pane)
        runCurrent()
        platform.socket.subscribed(2)
        platform.socket.output("initial", snapshot = true)
        platform.socket.output("later")
        runCurrent()
        val feed = assertNotNull(controller.state.value.terminal)
        assertFalse(controller.state.value.terminalSynced)
        val full = assertNotNull(feed.stage(feed.frames.receive()))
        assertFalse(controller.state.value.terminalSynced)
        feed.acknowledgeParsed(full)
        assertTrue(controller.state.value.terminalSynced)
        assertEquals(1, platform.socket.acknowledgments.size)
        assertEquals("later", feed.frames.receive().lines.first().ansi)
        controller.setForeground(false)
    }

    @Test fun staleCompletionCannotRevealReopenedSamePane() = runTest {
        val platform = ControlledPlatform(listOf(approved), pending)
        val controller = authenticatedController(platform)
        val original = openTerminal(controller, platform.socket)
        platform.socket.output("old-unparsed")
        runCurrent()
        val oldFrame = original.frames.tryReceive().getOrThrow()
        controller.selectTerminal("workspace", pane)
        assertFalse(controller.state.value.terminalSynced)
        runCurrent()
        platform.socket.response(3, JsonNull)
        runCurrent()
        platform.socket.subscribed(4, "new-generation")
        platform.socket.output("new-snapshot", snapshot = true)
        
        runCurrent()
        val current = assertNotNull(controller.state.value.terminal)
        original.acknowledgeParsed(oldFrame)
        current.acknowledgeParsed(current.frames.tryReceive().getOrThrow())
        assertTrue(controller.state.value.connected)
        
        runCurrent()
        assertTrue(controller.state.value.terminalSynced)
        assertEquals("new-generation", platform.socket.acknowledgments.single().text("subscription_id"))
        controller.setForeground(false)
    }


    @Test fun latestSelectionWinsWhileOldUnsubscribeAndCanceledSubscribeResponsesAreQueued() = runTest {
        val platform = ControlledPlatform(listOf(approved), pending)
        val controller = authenticatedController(platform)
        openTerminal(controller, platform.socket)
        val second = pane.copy(id = 2, title = "second")
        val latest = pane.copy(id = 3, title = "latest")
        controller.selectTerminal("workspace", second)
        assertEquals("workspace" to second, controller.state.value.terminalSelection)
        assertNull(controller.state.value.terminal)
        runCurrent()
        controller.selectTerminal("workspace", latest)
        assertEquals("workspace" to latest, controller.state.value.terminalSelection)
        runCurrent()
        platform.socket.response(3, JsonNull)
        platform.socket.response(4, JsonNull)
        runCurrent()
        val selected = assertNotNull(controller.state.value.terminal)
        assertEquals(latest, selected.pane)
        platform.socket.subscribed(5, "latest-generation")
        platform.socket.output("latest-snapshot", snapshot = true, paneId = latest.id)
        
        runCurrent()
        assertFalse(controller.state.value.terminalSynced)
        selected.acknowledgeParsed(selected.frames.tryReceive().getOrThrow())
        assertTrue(controller.state.value.terminalSynced)
        assertEquals(listOf(1L, 3L), platform.socket.encryptedMessages.filter { it.optionalText("op") == "subscribe" }
            .map { it.getValue("pane_id").jsonPrimitive.long })
        assertTrue(controller.state.value.connected)
        controller.setForeground(false)
    }

    @Test fun leaveInvalidatesBothPendingSubscribeAndSelectionQueuedBehindSwitch() = runTest {
        val platform = ControlledPlatform(listOf(approved), pending)
        val controller = authenticatedController(platform)
        controller.selectTerminal("workspace", pane)
        runCurrent()
        controller.selectTerminal("workspace", pane.copy(id = 2))
        controller.leaveTerminal()
        assertNull(controller.state.value.terminalSelection)
        assertNull(controller.state.value.terminal)
        assertFalse(controller.state.value.terminalSynced)
        runCurrent()
        platform.socket.subscribed(2, "canceled-generation")
        renderEmpty(controller, platform.socket)
        platform.socket.response(3, JsonNull)
        runCurrent()
        assertNull(controller.state.value.terminalSelection)
        assertNull(controller.state.value.terminal)
        assertEquals(1, platform.socket.encryptedMessages.count { it.optionalText("op") == "subscribe" })
        assertTrue(controller.state.value.connected)
        controller.setForeground(false)
    }

    @Test fun transientOfflineKeepsTargetAndResumedGenerationWaitsForExactCompletion() = runTest {
        val platform = ControlledPlatform(listOf(approved), pending)
        val controller = authenticatedController(platform)
        val original = openTerminal(controller, platform.socket)
        platform.socket.close()
        runCurrent()
        assertEquals("workspace" to pane, controller.state.value.terminalSelection)
        assertNull(controller.state.value.terminal)
        assertFalse(controller.state.value.terminalSynced)
        val resumed = platform.nextSocket().apply { authenticate() }
        advanceTimeBy(1_001)
        runCurrent()
        resumed.response(3, listing())
        resumed.subscribed(4, "network-restored-generation")
        resumed.output("restored-snapshot", snapshot = true)
        runCurrent()
        val current = assertNotNull(controller.state.value.terminal)
        assertNotSame(original, current)
        current.acknowledgeParsed(current.frames.tryReceive().getOrThrow())
        assertTrue(controller.state.value.terminalSynced)
        assertEquals("workspace" to pane, controller.state.value.terminalSelection)
        assertTrue(controller.state.value.connected)
        controller.setForeground(false)
    }

    @Test fun terminalExitClearsSelectionWithoutResurrectingOldIntent() = runTest {
        val platform = ControlledPlatform(listOf(approved), pending)
        val controller = authenticatedController(platform)
        val feed = openTerminal(controller, platform.socket)
        platform.socket.encrypted(obj("type" to JsonPrimitive("exited"), "pane_id" to JsonPrimitive(pane.id),
            "subscription_id" to JsonPrimitive(feed.subscriptionId!!), "code" to JsonPrimitive(0)))
        runCurrent()
        assertNull(controller.state.value.terminalSelection)
        assertNull(controller.state.value.terminal)
        assertFalse(controller.state.value.terminalSynced)
        assertTrue(controller.state.value.connected)
        controller.setForeground(false)
    }

    @Test fun forgettingSelectedComputerClearsLoadingTargetAndStopsQueuedSelection() = runTest {
        val platform = ControlledPlatform(listOf(approved), pending)
        val controller = authenticatedController(platform)
        controller.selectTerminal("workspace", pane)
        runCurrent()
        controller.forget(approved)
        runCurrent()
        assertNull(controller.state.value.terminalSelection)
        assertNull(controller.state.value.terminal)
        assertFalse(controller.state.value.terminalSynced)
        assertTrue(platform.disk.isEmpty())
        assertFalse(controller.state.value.connected)
        controller.setForeground(false)
    }

    @Test fun cancelingNativeAckInsideEncryptionStillCommitsCapturedLiveSessionSequence() = runTest {
        val platform = ControlledPlatform(listOf(approved), pending)
        val controller = authenticatedController(platform)
        val original = openTerminal(controller, platform.socket)
        platform.socket.output("rendered-old-feed")
        runCurrent()
        val frame = original.frames.tryReceive().getOrThrow()
        lateinit var rendering: Job
        platform.afterEncrypt = { message ->
            if (message.optionalText("type") == "output_ack") {
                rendering.cancel()
                controller.selectTerminal("workspace", pane)
            }
        }
        rendering = backgroundScope.launch { original.acknowledgeParsed(frame) }
        runCurrent()
        assertTrue(rendering.isCancelled)
        assertEquals(frame.subscriptionId, platform.socket.acknowledgments.single().text("subscription_id"))
        assertEquals("unsubscribe", platform.socket.encryptedMessages.last().text("op"))
        assertFalse(platform.socket.closed)
        platform.afterEncrypt = {}
        platform.socket.response(3, JsonNull)
        runCurrent()
        platform.socket.subscribed(4, "after-canceled-ack")
        renderEmpty(controller, platform.socket)
        runCurrent()
        assertTrue(controller.state.value.connected)
        assertTrue(controller.state.value.terminalSynced)
        assertNull(controller.state.value.error)
        controller.setForeground(false)
    }

    @Test fun cancelingControlRequestInsideEncryptionDoesNotSkipNextLiveSessionSequence() = runTest {
        val platform = ControlledPlatform(listOf(approved), pending)
        val controller = authenticatedController(platform)
        lateinit var refresh: Job
        platform.afterEncrypt = { message ->
            if (message.optionalText("op") == "list") refresh.cancel()
        }
        refresh = assertNotNull(controller.refresh())
        runCurrent()
        assertTrue(refresh.isCancelled)
        assertEquals(2L, platform.socket.encryptedMessages.last().getValue("id").jsonPrimitive.long)
        platform.afterEncrypt = {}
        platform.socket.response(2, listing())
        controller.selectTerminal("workspace", pane)
        runCurrent()
        platform.socket.subscribed(3, "after-canceled-control")
        renderEmpty(controller, platform.socket)
        runCurrent()
        assertTrue(controller.state.value.connected)
        assertTrue(controller.state.value.terminalSynced)
        assertFalse(platform.socket.closed)
        assertNull(controller.state.value.error)
        controller.setForeground(false)
    }

    @Test fun leavingDuringSubscribeEncryptionCommitsEnvelopeButCannotPublishCanceledTarget() = runTest {
        val platform = ControlledPlatform(listOf(approved), pending)
        val controller = authenticatedController(platform)
        platform.afterEncrypt = { message ->
            if (message.optionalText("op") == "subscribe") controller.leaveTerminal()
        }
        controller.selectTerminal("workspace", pane)
        runCurrent()
        assertNull(controller.state.value.terminalSelection)
        assertNull(controller.state.value.terminal)
        assertEquals(listOf("subscribe", "unsubscribe"), platform.socket.encryptedMessages
            .mapNotNull { it.optionalText("op") }.takeLast(2))
        platform.afterEncrypt = {}
        platform.socket.subscribed(2, "canceled-during-encryption")
        renderEmpty(controller, platform.socket)
        platform.socket.response(3, JsonNull)
        runCurrent()
        assertNull(controller.state.value.terminalSelection)
        assertFalse(controller.state.value.terminalSynced)
        assertTrue(controller.state.value.connected)
        assertFalse(platform.socket.closed)
        controller.setForeground(false)
    }

    @Test fun failureAfterConsumingEncryptedSequenceRetiresSessionBeforeReconnect() = runTest {
        val platform = ControlledPlatform(listOf(approved), pending)
        val controller = authenticatedController(platform)
        val original = openTerminal(controller, platform.socket)
        platform.socket.output("parsed-before-encryption-error")
        runCurrent()
        val frame = original.frames.tryReceive().getOrThrow()
        platform.afterEncrypt = { message ->
            if (message.optionalText("type") == "output_ack") error("Encryption failed after sequence reservation")
        }
        var failure: Throwable? = null
        backgroundScope.launch {
            try { original.acknowledgeParsed(frame) }
            catch (e: Exception) { failure = e }
        }
        runCurrent()
        assertEquals("Encryption failed after sequence reservation", assertNotNull(failure).message)
        assertTrue(platform.socket.closed)
        assertFalse(controller.state.value.connected)
        assertEquals("workspace" to pane, controller.state.value.terminalSelection)
        assertFalse(controller.state.value.terminalSynced)
        platform.afterEncrypt = {}
        val resumed = platform.nextSocket().apply { authenticate() }
        advanceTimeBy(1_001)
        runCurrent()
        resumed.response(3, listing())
        resumed.subscribed(4, "after-retired-encryption-session")
        renderEmpty(controller, resumed)
        runCurrent()
        assertTrue(controller.state.value.connected)
        assertTrue(controller.state.value.terminalSynced)
        assertTrue(resumed.acknowledgments.isEmpty())
        controller.setForeground(false)
    }

    @Test fun networkLossImmediatelyMasksTargetAndRouteReplacementResubscribesFreshGeneration() = runTest {
        val platform = ControlledPlatform(listOf(approved), pending)
        val controller = authenticatedController(platform)
        val original = openTerminal(controller, platform.socket)
        platform.socket.output("old-unparsed")
        runCurrent()
        val oldFrame = original.frames.tryReceive().getOrThrow()
        controller.networkChanged(false)
        assertFalse(controller.state.value.connected)
        assertFalse(controller.state.value.terminalSynced)
        assertNull(controller.state.value.terminal)
        assertEquals("workspace" to pane, controller.state.value.terminalSelection)
        assertTrue(platform.socket.closed)
        controller.input("must-not-send-offline")
        runCurrent()
        original.acknowledgeParsed(oldFrame)
        assertTrue(platform.socket.acknowledgments.isEmpty())
        advanceTimeBy(60_000)
        runCurrent()
        controller.setForeground(false)
        controller.setForeground(true)
        runCurrent()
        assertEquals(1, platform.connections.size)
        assertEquals("workspace" to pane, controller.state.value.terminalSelection)
        val restored = platform.nextSocket().apply { authenticate() }
        controller.networkChanged(true)
        runCurrent()
        restored.response(3, listing())
        restored.subscribed(4, "restored-default-route")
        restored.output("restored-history", snapshot = true)
        
        runCurrent()
        val current = assertNotNull(controller.state.value.terminal)
        assertNotSame(original, current)
        assertFalse(controller.state.value.terminalSynced)
        current.acknowledgeParsed(current.frames.tryReceive().getOrThrow())
        assertTrue(controller.state.value.terminalSynced)
        val replacementRoute = platform.nextSocket().apply { authenticate() }
        controller.networkChanged(true)
        assertFalse(controller.state.value.connected)
        assertFalse(controller.state.value.terminalSynced)
        assertEquals("workspace" to pane, controller.state.value.terminalSelection)
        runCurrent()
        replacementRoute.response(5, listing())
        replacementRoute.subscribed(6, "replaced-default-route")
        runCurrent()
        assertFalse(controller.state.value.terminalSynced)
        renderEmpty(controller, replacementRoute)
        runCurrent()
        assertEquals(3, platform.connections.size)
        assertTrue(controller.state.value.connected)
        assertTrue(controller.state.value.terminalSynced)
        controller.setForeground(false)
    }

    @Test fun networkNotificationsRespectExplicitDisconnectAndManualOfflineConnectWaitsForRoute() = runTest {
        val platform = ControlledPlatform(listOf(approved), pending)
        val controller = authenticatedController(platform)
        openTerminal(controller, platform.socket)
        controller.disconnect()
        controller.networkChanged(false)
        controller.networkChanged(true)
        runCurrent()
        assertEquals(1, platform.connections.size)
        assertNull(controller.state.value.terminalSelection)
        assertFalse(controller.state.value.connected)
        controller.networkChanged(false)
        controller.connect(approved)
        runCurrent()
        assertEquals(1, platform.connections.size)
        assertFalse(controller.state.value.connected)
        val restored = platform.nextSocket().apply { authenticate() }
        controller.networkChanged(true)
        runCurrent()
        restored.response(3, listing())
        runCurrent()
        assertEquals(2, platform.connections.size)
        assertTrue(controller.state.value.connected)
        assertNull(controller.state.value.terminalSelection)
        controller.setForeground(false)
    }

    @Test fun pairingStartedOfflineCanHandshakeWhenInternetCapableRouteReturns() = runTest {
        val platform = ControlledPlatform(emptyList(), pending)
        val controller = controller(platform)
        controller.networkChanged(false)
        controller.setForeground(true)
        controller.pair("qr")
        runCurrent()
        assertTrue(platform.connections.isEmpty())
        assertEquals(pending, controller.state.value.computer)
        platform.socket.handshake()
        controller.networkChanged(true)
        runCurrent()
        assertEquals(listOf(pending), platform.connections)
        assertEquals("pair", platform.socket.encryptedMessages.single().text("type"))
        controller.setForeground(false)
    }

    @Test fun prewarmedGeometryIsIncludedBeforeInitialSubscription() = runTest {
        val platform = ControlledPlatform(listOf(approved), pending)
        val controller = authenticatedController(platform)
        controller.measuredPhoneGrid(42, 28)
        controller.selectTerminal("workspace", pane)
        runCurrent()
        val request = platform.socket.encryptedMessages.single { it.optionalText("op") == "subscribe" }
        assertEquals(42, request.getValue("cols").jsonPrimitive.int)
        assertEquals(28, request.getValue("rows").jsonPrimitive.int)
        assertTrue(platform.socket.encryptedMessages.none { it.optionalText("op") == "terminal_size" })
        controller.setForeground(false)
    }

    @Test fun historyRequestsUseSubscriptionAndCursorWithoutConsumingLiveCredits() = runTest {
        val platform = ControlledPlatform(listOf(approved), pending)
        val controller = authenticatedController(platform)
        val feed = openTerminal(controller, platform.socket)
        val history = backgroundScope.async { feed.history(null) }
        runCurrent()
        val request = platform.socket.encryptedMessages.single { it.optionalText("op") == "history" }
        assertEquals("workspace", request.text("workspace_id"))
        assertEquals(pane.id, request.getValue("pane_id").jsonPrimitive.long)
        assertEquals(feed.subscriptionId, request.text("subscription_id"))
        assertEquals(JsonNull, request["cursor"])
        platform.socket.output("live-while-history-pending")
        runCurrent()
        feed.acknowledgeParsed(assertNotNull(feed.stage(feed.frames.receive())))
        assertEquals(1, platform.socket.acknowledgments.size)
        platform.socket.response(request.getValue("id").jsonPrimitive.long, obj(
            "lines" to JsonArray(listOf(obj("text" to JsonPrimitive("old"), "ansi" to JsonPrimitive("\u001b[31mold"), "cols" to JsonPrimitive(80)))),
            "cursor" to JsonPrimitive("opaque-next"), "has_more" to JsonPrimitive(true), "truncated" to JsonPrimitive(false)))
        runCurrent()
        val page = history.await()
        assertEquals("old", page.lines.single().jsonObject.text("text"))
        val older = backgroundScope.async { runCatching { feed.history(page.cursor) } }
        runCurrent()
        val next = platform.socket.encryptedMessages.last { it.optionalText("op") == "history" }
        assertEquals("opaque-next", next.text("cursor"))
        platform.socket.encrypted(obj("type" to JsonPrimitive("response"), "id" to next.getValue("id"),
            "ok" to JsonPrimitive(false), "error" to JsonPrimitive("History cursor expired")))
        runCurrent()
        assertEquals("History cursor expired", older.await().exceptionOrNull()?.message)
        assertTrue(controller.state.value.connected)
        assertTrue(feed.synced)
        controller.setForeground(false)
    }

    @Test fun wrongScreenBaseSchedulesResyncForCurrentSubscription() = runTest {
        val platform = ControlledPlatform(listOf(approved), pending)
        val controller = authenticatedController(platform)
        val feed = openTerminal(controller, platform.socket)
        platform.socket.output("wrong-base")
        runCurrent()
        val incoming = feed.frames.receive()
        assertNull(feed.stage(incoming.copy(baseRevision = "0")))
        runCurrent()
        val request = platform.socket.encryptedMessages.single { it.optionalText("op") == "resync" }
        assertEquals(feed.subscriptionId, request.text("subscription_id"))
        assertEquals(pane.id, request.getValue("pane_id").jsonPrimitive.long)
        assertEquals(1, platform.socket.acknowledgments.size)
        platform.socket.response(request.getValue("id").jsonPrimitive.long, JsonNull)
        platform.socket.output("replacement", snapshot = true)
        runCurrent()
        feed.acknowledgeParsed(assertNotNull(feed.stage(feed.frames.receive())))
        assertTrue(feed.synced)
        controller.setForeground(false)
    }

    private fun listing(status: String = "idle", group: String = "Project", branch: String = "main", revision: Long = 0) = obj(
        "workspaces" to JsonArray(listOf(obj("id" to JsonPrimitive("workspace"), "name" to JsonPrimitive("Workspace"),
            "terminals" to JsonArray(listOf(obj("pane_id" to JsonPrimitive(1), "title" to JsonPrimitive("shell"),
                "cols" to JsonPrimitive(80), "rows" to JsonPrimitive(24), "tab_id" to JsonPrimitive("tab"),
                "group" to JsonPrimitive(group), "group_id" to JsonPrimitive("pinned:project"), "git_branch" to JsonPrimitive(branch),
                "agent_revision" to JsonPrimitive(revision),
                "agent" to obj("status" to JsonPrimitive(status), "name" to JsonPrimitive("Pi"))))))))
    )
    @Test fun phoneResizeWaitsForParsedSnapshotAndUsesCurrentSubscriptionIdentity() = runTest {
        val platform = ControlledPlatform(listOf(approved), pending)
        val controller = authenticatedController(platform)
        controller.selectTerminal("workspace", pane)
        runCurrent()
        val feed = assertNotNull(controller.state.value.terminal)
        assertNull(controller.resizeTerminal(feed, 42, 28))
        platform.socket.subscribed(2)
        platform.socket.output("initial", snapshot = true)
        
        runCurrent()
        assertNull(controller.resizeTerminal(feed, 42, 28))
        feed.acknowledgeParsed(feed.frames.tryReceive().getOrThrow())
        val resize = assertNotNull(controller.resizeTerminal(feed, 42, 28))
        runCurrent()
        val request = platform.socket.encryptedMessages.single { it.optionalText("op") == "terminal_size" }
        assertEquals("workspace", request.text("workspace_id"))
        assertEquals(pane.id, request.getValue("pane_id").jsonPrimitive.long)
        assertEquals(feed.subscriptionId, request.text("subscription_id"))
        assertEquals(42, request.getValue("cols").jsonPrimitive.int)
        assertEquals(28, request.getValue("rows").jsonPrimitive.int)
        platform.socket.response(request.getValue("id").jsonPrimitive.long, JsonNull)
        runCurrent()
        assertTrue(resize.isCompleted)
        controller.leaveTerminal()
        assertNull(controller.resizeTerminal(feed, 45, 18))
        runCurrent()
        assertEquals(1, platform.socket.encryptedMessages.count { it.optionalText("op") == "terminal_size" })
        controller.setForeground(false)
    }

    @Test fun desktopTakebackIsVisibleStickyAndDoesNotStopTerminalInput() = runTest {
        val platform = ControlledPlatform(listOf(approved), pending)
        val controller = authenticatedController(platform)
        val feed = openTerminal(controller, platform.socket)
        fun lease(subscription: String, held: Boolean) = platform.socket.encrypted(obj(
            "type" to JsonPrimitive("terminal_lease"), "pane_id" to JsonPrimitive(pane.id),
            "subscription_id" to JsonPrimitive(subscription), "held" to JsonPrimitive(held),
        ))
        lease("retired-subscription", false)
        runCurrent()
        assertFalse(feed.sizeLeaseLost)
        assertNull(controller.state.value.error)
        lease(feed.subscriptionId!!, false)
        runCurrent()
        assertTrue(feed.sizeLeaseLost)
        assertTrue(controller.state.value.error.orEmpty().contains("Desktop took back"))
        assertTrue(controller.state.value.connected)
        assertTrue(controller.state.value.terminalSynced)
        assertSame(feed, controller.state.value.terminal)
        assertNull(controller.resizeTerminal(feed, 42, 28))
        lease(feed.subscriptionId!!, true)
        runCurrent()
        assertTrue(feed.sizeLeaseLost)
        assertNull(controller.resizeTerminal(feed, 45, 18))
        controller.input("still-readable-and-writable")
        runCurrent()
        assertEquals(1, platform.socket.encryptedMessages.count { it.optionalText("op") == "input" })
        assertEquals(0, platform.socket.encryptedMessages.count { it.optionalText("op") == "terminal_size" })
        controller.setForeground(false)
    }

    @Test fun resizeBoundsAndHostRefusalRemainVisibleWithoutDisconnecting() = runTest {
        val platform = ControlledPlatform(listOf(approved), pending)
        val controller = authenticatedController(platform)
        val feed = openTerminal(controller, platform.socket)
        for ((cols, rows) in listOf(19 to 28, 42 to 4, 1001 to 28, 42 to 1001)) {
            controller.resizeTerminal(feed, cols, rows)
            runCurrent()
            assertTrue(controller.state.value.error.orEmpty().contains("Terminal size"))
            assertTrue(controller.state.value.connected)
            assertSame(feed, controller.state.value.terminal)
            controller.clearError()
        }
        assertEquals(0, platform.socket.encryptedMessages.count { it.optionalText("op") == "terminal_size" })
        controller.resizeTerminal(feed, 42, 28)
        runCurrent()
        val request = platform.socket.encryptedMessages.single { it.optionalText("op") == "terminal_size" }
        platform.socket.encrypted(obj("type" to JsonPrimitive("response"),
            "id" to request.getValue("id"), "ok" to JsonPrimitive(false),
            "error" to JsonPrimitive("terminal size control was taken back")))
        runCurrent()
        assertEquals("terminal size control was taken back", controller.state.value.error)
        assertTrue(controller.state.value.connected)
        assertTrue(feed.active)
        controller.setForeground(false)
    }

    private fun TestScope.controller(platform: ControlledPlatform) = RemoteController(
        platform, backgroundScope, StandardTestDispatcher(testScheduler), handshakeTimeout = 1_000, requestTimeout = 1_000,
    )
    private fun TestScope.authenticatedController(platform: ControlledPlatform): RemoteController {
        platform.socket.authenticate()
        val controller = controller(platform)
        controller.setForeground(true)
        runCurrent()
        platform.socket.response(1, listing())
        runCurrent()
        assertTrue(controller.state.value.connected)
        return controller
    }
    private suspend fun TestScope.openTerminal(controller: RemoteController, socket: ControlledSocket): TerminalFeed {
        controller.selectTerminal("workspace", pane)
        runCurrent()
        socket.subscribed(2)
        renderEmpty(controller, socket)
        runCurrent()
        return assertNotNull(controller.state.value.terminal)
    }
    private suspend fun TestScope.renderEmpty(controller: RemoteController, socket: ControlledSocket, paneId: Long = 1) {
        socket.output("", snapshot = true, paneId = paneId)
        runCurrent()
        val feed = controller.state.value.terminal
        if (feed != null && feed.active && feed.pane.id == paneId) {
            feed.acknowledgeParsed(assertNotNull(feed.stage(feed.frames.receive())))
        }
        // Bootstrap is exercised above; subsequent assertions observe new ACKs.
        socket.observeAcknowledgments()
    }

    // Controlled delivery exercises controller transitions and channel ownership;
    // Android TLS/Keystore is covered by runtime smoke, not this state fixture.
    private class ControlledPlatform(initial: List<Computer>, private val qr: Computer) : MobilePlatform {
        var disk = initial
        var loadRelease: CompletableDeferred<Unit>? = null
        var afterCommit: suspend (List<Computer>) -> Unit = {}
        var afterEncrypt: (JsonObject) -> Unit = {}
        val connections = mutableListOf<Computer>()
        val socket = ControlledSocket()
        private val sockets = mutableListOf(socket)
        fun nextSocket() = ControlledSocket().also { sockets.add(it) }
        override val deviceName = "Phone"
        override fun parseQr(uri: String) = qr
        override suspend fun loadComputers(): List<Computer> { loadRelease?.await(); return disk }
        override suspend fun saveComputers(computers: List<Computer>) { disk = computers; afterCommit(computers) }
        override suspend fun connect(computer: Computer): RemoteSocket {
            val index = connections.size
            connections.add(computer)
            return sockets.getOrElse(index) { nextSocket() }
        }
        override fun randomNonce() = "nonce"
        override fun encodeInput(text: String) = text
        override fun nowSeconds() = 0L
        override fun deriveSession(secret: String, clientNonce: String, hostNonce: String) = object : SecureSession {
            override val verificationCode = "12345678"
            private var nextSequence = 1L
            override fun encrypt(plain: String): JsonObject {
                val envelope = obj("kind" to JsonPrimitive("encrypted"), "plain" to JsonPrimitive(plain),
                    "sequence" to JsonPrimitive(nextSequence++))
                afterEncrypt(wireJson.parseToJsonElement(plain).jsonObject)
                return envelope
            }
            override fun decrypt(envelope: JsonObject) = envelope.text("plain")
        }
    }
    private class ControlledSocket : RemoteSocket {
        val incoming = Channel<String>(Channel.UNLIMITED)
        private val sent = mutableListOf<JsonObject>()
        private var subscriptionId = "subscription-1"
        private var nextSequence = 1L
        private var nextEncryptedSequence = 1L
        val encryptedMessages: List<JsonObject>
            get() = sent.filter { it.optionalText("type") == "data" }
                .map { wireJson.parseToJsonElement(it.text("payload")).jsonObject }
                .filter { it.optionalText("kind") == "encrypted" }
                .map { wireJson.parseToJsonElement(it.text("plain")).jsonObject }
        private var ackBaseline = 0
        val acknowledgments: List<JsonObject>
            get() = encryptedMessages.filter { it.optionalText("type") == "output_ack" }.drop(ackBaseline)
        fun observeAcknowledgments() { ackBaseline = encryptedMessages.count { it.optionalText("type") == "output_ack" } }
        var closed = false
        override suspend fun send(message: String) {
            check(!closed)
            val outer = wireJson.parseToJsonElement(message).jsonObject
            if (outer.optionalText("type") == "data") {
                val inner = wireJson.parseToJsonElement(outer.text("payload")).jsonObject
                if (inner.optionalText("kind") == "encrypted") {
                    check(inner.getValue("sequence").jsonPrimitive.long == nextEncryptedSequence++) { "Encrypted send sequence skipped" }
                }
            }
            sent.add(outer)
        }
        override suspend fun receive() = incoming.receive()
        override fun close() { closed = true; incoming.close() }
        fun handshake(protocol: JsonElement? = JsonPrimitive(MOBILE_PROTOCOL)) {
            incoming.trySend(obj("type" to JsonPrimitive("ready"), "client_id" to JsonPrimitive("client")).toString())
            inner(buildJsonObject {
                put("kind", "challenge")
                put("nonce", "host-nonce")
                protocol?.let { put("protocol", it) }
            })
        }
        fun authenticate(name: String = "Computer") {
            handshake()
            encrypted(obj("type" to JsonPrimitive("authenticated"), "host_name" to JsonPrimitive(name)))
        }
        private fun inner(value: JsonObject) { incoming.trySend(obj("type" to JsonPrimitive("data"), "payload" to JsonPrimitive(value.toString())).toString()) }
        fun encrypted(value: JsonObject) = inner(obj("kind" to JsonPrimitive("encrypted"), "plain" to JsonPrimitive(value.toString())))
        fun response(id: Long, result: JsonElement) = encrypted(obj("type" to JsonPrimitive("response"), "id" to JsonPrimitive(id), "ok" to JsonPrimitive(true), "result" to result))
        fun subscribed(id: Long, subscriptionId: String = "subscription-1") {
            this.subscriptionId = subscriptionId
            nextSequence = 1
            response(id, obj("subscription_id" to JsonPrimitive(subscriptionId), "cols" to JsonPrimitive(80), "rows" to JsonPrimitive(24)))
        }
        fun agent(status: String?, revision: Long, subscription: String = subscriptionId) = encrypted(obj(
            "type" to JsonPrimitive("agent"), "pane_id" to JsonPrimitive(1),
            "subscription_id" to JsonPrimitive(subscription), "agent_revision" to JsonPrimitive(revision),
            "agent" to (status?.let { obj("status" to JsonPrimitive(it), "name" to JsonPrimitive("Pi")) } ?: JsonNull)))
        fun output(
            data: String, snapshot: Boolean = false, subscriptionId: String = this.subscriptionId, sequence: Long = nextSequence++, paneId: Long = 1,
        ) = encrypted(obj("type" to JsonPrimitive("screen"), "pane_id" to JsonPrimitive(paneId),
            "subscription_id" to JsonPrimitive(subscriptionId), "sequence" to JsonPrimitive(sequence),
            "revision" to JsonPrimitive(sequence.toString()),
            "base_revision" to (if (snapshot) JsonNull else JsonPrimitive((sequence - 1).toString())),
            "epoch" to JsonPrimitive("1"), "cols" to JsonPrimitive(80), "rows" to JsonPrimitive(24),
            "alternate" to JsonPrimitive(false),
            "cursor" to obj("row" to JsonPrimitive(0), "col" to JsonPrimitive(0), "shape" to JsonPrimitive("block"),
                "visible" to JsonPrimitive(true), "blink" to JsonPrimitive(true)),
            "modes" to obj("app_cursor" to JsonPrimitive(false), "app_keypad" to JsonPrimitive(false), "bracketed_paste" to JsonPrimitive(false)),
            "lines" to JsonArray((if (snapshot) 0 until 24 else 0..0).map { row ->
                obj("row" to JsonPrimitive(row), "ansi" to JsonPrimitive(if (row == 0) data else ""))
            }), "part" to JsonPrimitive(0), "last" to JsonPrimitive(true)))
    }
}
