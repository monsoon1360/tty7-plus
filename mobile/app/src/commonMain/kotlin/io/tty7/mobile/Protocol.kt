package io.tty7.mobile

import androidx.compose.runtime.Composable
import kotlinx.coroutines.channels.Channel
import kotlinx.coroutines.currentCoroutineContext
import kotlinx.coroutines.ensureActive
import kotlinx.serialization.json.*

const val DEFAULT_RELAY = "wss://relay.example.invalid:8443/v1/ws"
const val MOBILE_PROTOCOL = 2
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
)
data class Workspace(val id: String, val name: String, val panes: List<Pane>)
data class TerminalFrame(
    val data: String, val snapshot: Boolean, val cols: Int, val rows: Int,
    val subscriptionId: String, val sequence: ULong,
)
class TerminalFeed(
    val workspace: String,
    val pane: Pane,
    private val onSynced: (TerminalFeed) -> Unit,
    private val onParsed: suspend (TerminalFeed, TerminalFrame) -> Unit,
) {
    val frames = Channel<TerminalFrame>(256)
    internal var subscriptionId: String? = null
        private set
    internal var active = true
        private set
    private var receivedSequence = 0uL
    private var parsedSequence = 0uL
    private var snapshotSequence: ULong? = null
    private var notifiedSynced = false
    internal val synced: Boolean
        get() = active && snapshotSequence?.let { parsedSequence >= it } == true
    private fun notifySynced() {
        if (!notifiedSynced && synced) {
            notifiedSynced = true
            onSynced(this)
        }
    }

    internal fun completeSnapshot(sequence: ULong) {
        check(active && subscriptionId != null) { "Terminal subscription is no longer active" }
        require(snapshotSequence == null && sequence == receivedSequence) { "Invalid terminal snapshot completion" }
        snapshotSequence = sequence
        notifySynced()
    }

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
        receivedSequence = frame.sequence
        frames.send(frame)
    }
    suspend fun acknowledgeParsed(frame: TerminalFrame) {
        currentCoroutineContext().ensureActive()
        if (!active) return
        check(frame.subscriptionId == subscriptionId && parsedSequence < ULong.MAX_VALUE &&
            frame.sequence == parsedSequence + 1uL && frame.sequence <= receivedSequence) {
            "Invalid parsed terminal frame"
        }
        onParsed(this, frame)
        parsedSequence = frame.sequence
        notifySynced()
    }
    fun cancel() { active = false; frames.cancel() }
    internal fun close() { active = false; frames.close() }
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
    fun parseQr(uri: String): Computer
    suspend fun loadComputers(): List<Computer>
    suspend fun saveComputers(computers: List<Computer>)
    suspend fun connect(computer: Computer): RemoteSocket
    fun randomNonce(): String
    fun deriveSession(secret: String, clientNonce: String, hostNonce: String): SecureSession
    fun encodeInput(text: String): String
    fun nowSeconds(): Long
}
@Composable expect fun CameraScanner(onScan: (String) -> Unit, onClose: () -> Unit)
@Composable expect fun TerminalSurface(feed: TerminalFeed, onInput: (String) -> Unit, onFailure: (String) -> Unit)
@Composable expect fun NavigationBackHandler(enabled: Boolean, onBack: () -> Unit)
