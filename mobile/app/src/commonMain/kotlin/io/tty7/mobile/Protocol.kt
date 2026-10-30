package io.tty7.mobile

import androidx.compose.runtime.Composable
import kotlinx.coroutines.channels.Channel
import kotlinx.coroutines.currentCoroutineContext
import kotlinx.coroutines.ensureActive
import kotlinx.serialization.json.*

const val DEFAULT_RELAY = "wss://relay.example.invalid:8443/v1/ws"
const val MOBILE_PROTOCOL = 3
val wireJson = Json { ignoreUnknownKeys = true }
fun obj(vararg fields: Pair<String, JsonElement>) = buildJsonObject { fields.forEach { (key, value) -> put(key, value) } }
fun JsonObject.text(key: String): String = getValue(key).jsonPrimitive.content
fun JsonObject.optionalText(key: String): String? = this[key]?.takeUnless { it is JsonNull }?.jsonPrimitive?.content

data class Computer(
    val hostId: String, val hostName: String, val relayUrl: String,
    val tlsFingerprint: String?, val credentialId: String, val secret: String,
    val expiresAt: Long? = null, val verificationCode: String? = null,
) {
    val pending: Boolean get() = credentialId.startsWith("pair:")
    fun toJson() = obj("host_id" to JsonPrimitive(hostId), "host_name" to JsonPrimitive(hostName),
        "relay_url" to JsonPrimitive(relayUrl), "tls_fingerprint" to (tlsFingerprint?.let(::JsonPrimitive) ?: JsonNull),
        "credential_id" to JsonPrimitive(credentialId), "secret" to JsonPrimitive(secret),
        "expires_at" to (expiresAt?.let(::JsonPrimitive) ?: JsonNull),
        "code" to (verificationCode?.let(::JsonPrimitive) ?: JsonNull))
    companion object {
        fun fromJson(value: JsonObject) = Computer(value.text("host_id"), value.text("host_name"),
            value.text("relay_url"), value.optionalText("tls_fingerprint"), value.text("credential_id"),
            value.text("secret"), value["expires_at"]?.takeUnless { it is JsonNull }?.jsonPrimitive?.long,
            value.optionalText("code"))
    }
}
data class Pane(
    val id: Long, val title: String, val cwd: String?, val cols: Int, val rows: Int,
    val tabId: String? = null, val group: String? = null, val gitBranch: String? = null,
    val agentStatus: String? = null, val agentName: String? = null, val groupId: String? = null,
    val agentRevision: ULong? = null,
)
data class Workspace(val id: String, val name: String, val panes: List<Pane>)
data class AgentNotification(
    val hostId: String, val workspaceId: String, val paneId: Long,
    val agentName: String, val title: String, val status: String,
)
data class ScreenLine(val row: Int, val ansi: String) {
    fun toJson() = obj("row" to JsonPrimitive(row), "ansi" to JsonPrimitive(ansi))
}
data class TerminalFrame(
    val revision: String, val baseRevision: String?, val epoch: String,
    val cols: Int, val rows: Int, val alternate: Boolean,
    val cursor: JsonObject, val modes: JsonObject, val lines: List<ScreenLine>,
    val subscriptionId: String, val sequence: ULong, val part: Int, val last: Boolean,
) {
    internal val presentationBytes: Int by lazy(LazyThreadSafetyMode.NONE) { lines.sumOf { it.ansi.encodeToByteArray().size } }
    fun presentation() = obj("revision" to JsonPrimitive(revision),
        "base_revision" to (baseRevision?.let(::JsonPrimitive) ?: JsonNull), "epoch" to JsonPrimitive(epoch),
        "cols" to JsonPrimitive(cols), "rows" to JsonPrimitive(rows), "alternate" to JsonPrimitive(alternate),
        "cursor" to cursor, "modes" to modes, "lines" to JsonArray(lines.map(ScreenLine::toJson)))
    companion object {
        fun fromJson(value: JsonObject): TerminalFrame {
            fun revision(key: String): String = value.getValue(key).jsonPrimitive.let {
                require(it.isString && it.content.toULongOrNull() != null) { "Invalid screen $key" }; it.content
            }
            val sequence = value.getValue("sequence").jsonPrimitive
            require(!sequence.isString && sequence.content.toULongOrNull()?.let { it > 0uL } == true) { "Invalid terminal output sequence" }
            val cols = value.getValue("cols").jsonPrimitive.int
            val rows = value.getValue("rows").jsonPrimitive.int
            require(cols in 1..1000 && rows in 1..1000) { "Invalid terminal dimensions" }
            val cursor = value.getValue("cursor").jsonObject
            require(cursor.getValue("row").jsonPrimitive.int in 0 until rows &&
                cursor.getValue("col").jsonPrimitive.int in 0 until cols &&
                cursor.text("shape") in setOf("block", "underline", "bar")) { "Invalid screen cursor" }
            cursor.getValue("visible").jsonPrimitive.boolean
            cursor.getValue("blink").jsonPrimitive.boolean
            val modes = value.getValue("modes").jsonObject
            listOf("app_cursor", "app_keypad", "bracketed_paste").forEach { modes.getValue(it).jsonPrimitive.boolean }
            return TerminalFrame(revision("revision"), value.optionalText("base_revision")?.also { revision("base_revision") },
                revision("epoch"), cols, rows, value.getValue("alternate").jsonPrimitive.boolean, cursor,
                modes, value.getValue("lines").jsonArray.map {
                    ScreenLine(it.jsonObject.getValue("row").jsonPrimitive.int, it.jsonObject.text("ansi"))
                }, value.text("subscription_id"), sequence.content.toULong(), value.getValue("part").jsonPrimitive.int,
                value.getValue("last").jsonPrimitive.boolean)
        }
    }
}
data class HistoryPage(val lines: JsonArray, val cursor: String?, val hasMore: Boolean, val truncated: Boolean) {
    fun toJson() = obj("lines" to lines, "cursor" to (cursor?.let(::JsonPrimitive) ?: JsonNull),
        "has_more" to JsonPrimitive(hasMore), "truncated" to JsonPrimitive(truncated))
    companion object {
        fun fromJson(value: JsonObject): HistoryPage {
            val lines = value.getValue("lines").jsonArray
            require(lines.size <= 200 && value.toString().encodeToByteArray().size <= 128 * 1024) { "History page too large" }
            lines.forEach {
                require(it.jsonObject.getValue("cols").jsonPrimitive.int in 1..1000)
                it.jsonObject.text("text")
                it.jsonObject.text("ansi")
            }
            val cursor = value.optionalText("cursor")
            val more = value.getValue("has_more").jsonPrimitive.boolean
            require(!more || !cursor.isNullOrBlank()) { "Missing history cursor" }
            return HistoryPage(lines, cursor, more, value.getValue("truncated").jsonPrimitive.boolean)
        }
    }
}
class TerminalFeed(
    val workspace: String,
    pane: Pane,
    private val onSynced: (TerminalFeed) -> Unit,
    private val onResync: (TerminalFeed) -> Unit = {},
    private val onHistory: suspend (TerminalFeed, String?) -> HistoryPage = { _, _ -> error("History unavailable") },
    private val onParsed: suspend (TerminalFeed, TerminalFrame) -> Unit,
) {
    var pane: Pane = pane
        private set
    internal fun updatePane(pane: Pane) {
        require(pane.id == this.pane.id) { "Terminal metadata belongs to another pane" }
        this.pane = pane
    }
    // The service has four credits; leave room for its bounded window only.
    val frames = Channel<TerminalFrame>(8)
    internal var subscriptionId: String? = null
        private set
    internal var active = true
        private set
    internal var sizeLeaseLost = false
        private set
    internal fun updateSizeLease(held: Boolean) { if (!held) sizeLeaseLost = true }
    private var receivedSequence = 0uL
    private var parsedSequence = 0uL
    private var revision: String? = null
    private var epoch: String? = null
    private var geometry: Pair<Int, Int>? = null
    private var alternate: Boolean? = null
    private var staged: TerminalFrame? = null
    private val stagedLines = mutableListOf<ScreenLine>()
    private var stagedBytes = 0
    private var nextPart = 0
    private var resyncPending = false
    private var notifiedSynced = false
    internal val synced: Boolean get() = active && notifiedSynced
    internal fun bindSubscription(id: String) {
        check(active && subscriptionId == null) { "Terminal subscription is no longer pending" }
        require(id.isNotBlank()) { "Invalid terminal subscription" }
        subscriptionId = id
    }
    internal fun identifiesSubscription(paneId: Long, subscriptionId: String) =
        pane.id == paneId && this.subscriptionId == subscriptionId
    internal fun matches(paneId: Long, subscriptionId: String) =
        active && identifiesSubscription(paneId, subscriptionId)
    internal suspend fun enqueue(frame: TerminalFrame) {
        check(active && frame.subscriptionId == subscriptionId) { "Terminal subscription changed" }
        require(receivedSequence < ULong.MAX_VALUE && frame.sequence == receivedSequence + 1uL) { "Invalid terminal output sequence" }
        require(frame.lines.size <= 1000 && frame.presentationBytes <= 128 * 1024) { "Screen part too large" }
        receivedSequence = frame.sequence
        frames.send(frame)
    }
    private fun clearStaged() { staged = null; stagedLines.clear(); stagedBytes = 0; nextPart = 0 }
    /** Stage bounded parts; only the returned complete state may reach the renderer. */
    suspend fun stage(frame: TerminalFrame): TerminalFrame? {
        if (!active || frame.subscriptionId != subscriptionId) return null
        val first = staged
        val validRevision = (revision == null || frame.revision.toULong() >= revision!!.toULong()) &&
            (epoch == null || frame.epoch.toULong() >= epoch!!.toULong()) &&
            (frame.baseRevision == null || frame.revision.toULong() > frame.baseRevision.toULong())
        val validBase = frame.baseRevision == null ||
            (!resyncPending && frame.baseRevision == revision && frame.epoch == epoch &&
                geometry == (frame.cols to frame.rows) && alternate == frame.alternate)
        val validPart = if (first == null) frame.part == 0 else
            frame.part == nextPart && first.revision == frame.revision && first.baseRevision == frame.baseRevision &&
                first.epoch == frame.epoch && first.cols == frame.cols && first.rows == frame.rows &&
                first.alternate == frame.alternate && first.cursor == frame.cursor && first.modes == frame.modes
        val bytes = frame.presentationBytes
        if (!validRevision || !validBase || !validPart || frame.cols !in 1..1000 || frame.rows !in 1..1000 ||
            frame.lines.any { it.row !in 0 until frame.rows } || stagedLines.size + frame.lines.size > frame.rows ||
            stagedBytes + bytes > 8 * 1024 * 1024 || frame.part > 1000) {
            clearStaged()
            acknowledge(frame, applied = false)
            if (!resyncPending) { resyncPending = true; onResync(this) }
            return null
        }
        if (first == null) staged = frame
        stagedLines.addAll(frame.lines)
        stagedBytes += bytes
        nextPart = frame.part + 1
        if (!frame.last) { acknowledge(frame, applied = false); return null }
        val rows = stagedLines.map { it.row }.toSet()
        if (rows.size != stagedLines.size || frame.baseRevision == null && rows.size != frame.rows) {
            clearStaged()
            acknowledge(frame, applied = false)
            if (!resyncPending) { resyncPending = true; onResync(this) }
            return null
        }
        val complete = frame.copy(lines = stagedLines.toList(), part = 0)
        clearStaged()
        return complete
    }
    suspend fun acknowledgeParsed(frame: TerminalFrame) = acknowledge(frame, applied = true)
    private suspend fun acknowledge(frame: TerminalFrame, applied: Boolean) {
        currentCoroutineContext().ensureActive()
        if (!active) return
        check(frame.subscriptionId == subscriptionId && parsedSequence < ULong.MAX_VALUE &&
            frame.sequence == parsedSequence + 1uL && frame.sequence <= receivedSequence) { "Invalid parsed terminal frame" }
        onParsed(this, frame)
        parsedSequence = frame.sequence
        if (applied) {
            revision = frame.revision; epoch = frame.epoch
            geometry = frame.cols to frame.rows; alternate = frame.alternate
            if (frame.baseRevision == null) {
                resyncPending = false
                if (!notifiedSynced) { notifiedSynced = true; onSynced(this) }
            }
        }
    }
    suspend fun history(cursor: String?): HistoryPage {
        check(synced) { "Terminal is not ready" }
        val page = onHistory(this, cursor)
        check(active) { "Terminal subscription changed" }
        return page
    }
    fun cancel() { active = false; clearStaged(); frames.cancel() }
    internal fun close() { active = false; clearStaged(); frames.close() }
}
interface SecureSession {
    val verificationCode: String
    fun encrypt(plain: String): JsonObject
    fun decrypt(envelope: JsonObject): String
}
interface RemoteSocket {
    suspend fun send(message: String)
    suspend fun receive(): String
    fun close()
}
interface MobilePlatform {
    val deviceName: String
    fun loadTheme(): MobileTheme?
    fun saveTheme(theme: MobileTheme)
    fun parseQr(uri: String): Computer
    suspend fun loadComputers(): List<Computer>
    suspend fun saveComputers(computers: List<Computer>)
    suspend fun connect(computer: Computer): RemoteSocket
    fun randomNonce(): String
    fun deriveSession(secret: String, clientNonce: String, hostNonce: String): SecureSession
    fun encodeInput(text: String): String
    fun nowSeconds(): Long
    fun notifyAgent(notification: AgentNotification)
}
@Composable expect fun CameraScanner(onScan: (String) -> Unit, onClose: () -> Unit)
@Composable expect fun TerminalSurface(feed: TerminalFeed?, synced: Boolean, onInput: (String) -> Unit, onResize: (Int, Int) -> Unit, onFailure: (String) -> Unit, shortcuts: kotlinx.coroutines.flow.Flow<String>, agentKind: String?)
@Composable expect fun NavigationBackHandler(enabled: Boolean, onBack: () -> Unit)
