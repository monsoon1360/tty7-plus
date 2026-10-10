package io.tty7.mobile

import android.content.Context
import android.net.Uri
import android.os.Build
import android.security.keystore.KeyGenParameterSpec
import android.security.keystore.KeyProperties
import android.util.Base64
import kotlinx.coroutines.*
import kotlinx.coroutines.channels.Channel
import kotlinx.coroutines.channels.trySendBlocking
import kotlinx.serialization.json.*
import okhttp3.*
import okio.ByteString
import java.net.URI
import java.nio.ByteBuffer
import java.security.KeyStore
import java.security.MessageDigest
import java.security.SecureRandom
import java.security.cert.X509Certificate
import java.util.concurrent.TimeUnit
import javax.crypto.Cipher
import javax.crypto.KeyGenerator
import javax.crypto.Mac
import javax.crypto.SecretKey
import javax.crypto.spec.GCMParameterSpec
import javax.crypto.spec.SecretKeySpec
import javax.net.ssl.SSLContext
import javax.net.ssl.X509TrustManager
import kotlin.coroutines.resume
import kotlin.coroutines.resumeWithException

private const val MAX_FRAME = 1024 * 1024
private const val KEY_ALIAS = "tty7.mobile.credentials.v1"
private const val C2H = "tty7-mobile-v1/c2h"
private const val H2C = "tty7-mobile-v1/h2c"
private fun encode(bytes: ByteArray) = Base64.encodeToString(bytes, Base64.URL_SAFE or Base64.NO_WRAP or Base64.NO_PADDING)
private fun decode(text: String): ByteArray {
    require(text.length <= MAX_FRAME && text.matches(Regex("[A-Za-z0-9_-]*"))) { "Invalid base64url" }
    return Base64.decode(text, Base64.URL_SAFE or Base64.NO_WRAP or Base64.NO_PADDING)
}
private fun nonce(text: String) = decode(text).also { require(it.size == 32) { "Invalid 32-byte secret/nonce" } }
private fun hex(bytes: ByteArray) = bytes.joinToString("") { "%02x".format(it.toInt() and 255) }
private fun fingerprint(value: String?) = value?.lowercase()?.also { require(it.matches(Regex("[0-9a-f]{64}"))) { "Invalid TLS SHA256 fingerprint" } }
private fun validateRelay(url: String, pin: String?) {
    val uri = URI(url)
    require(uri.rawUserInfo == null && uri.rawQuery == null && uri.rawFragment == null && uri.path == "/v1/ws" && !uri.host.isNullOrBlank()) { "Invalid relay URL" }
    when (uri.scheme) {
        "wss" -> require(fingerprint(pin) != null) { "TLS certificate pin is required" }
        "ws" -> require(BuildConfig.DEBUG && uri.host in setOf("localhost", "127.0.0.1", "[::1]", "::1", "10.0.2.2")) { "Unencrypted WebSocket is only allowed for loopback in debug builds" }
        else -> error("Relay must use wss")
    }
}
class AndroidPlatform(context: Context) : MobilePlatform {
    private val app = context.applicationContext
    private val random = SecureRandom()
    override val deviceName: String get() = "${Build.MANUFACTURER} ${Build.MODEL}".take(128)
    override fun nowSeconds() = System.currentTimeMillis() / 1000
    override fun randomNonce() = encode(ByteArray(32).also(random::nextBytes))
    override fun encodeInput(text: String) = encode(text.toByteArray(Charsets.UTF_8))
    override fun deriveSession(secret: String, clientNonce: String, hostNonce: String): SecureSession = AndroidSecureSession(nonce(secret), nonce(clientNonce), nonce(hostNonce))
    override fun parseQr(uri: String): Computer {
        require(uri.length <= 16_384) { "Pairing URI too large" }
        val parsed = Uri.parse(uri)
        require(parsed.scheme == "tty7" && parsed.host == "pair" && parsed.path.isNullOrEmpty() && parsed.fragment == null) { "Expected tty7://pair URI" }
        val data = parsed.getQueryParameter("data") ?: error("Missing pairing data")
        val value = wireJson.parseToJsonElement(decode(data).toString(Charsets.UTF_8)).jsonObject
        require(value.getValue("v").jsonPrimitive.int == 1) { "Unsupported pairing version" }
        val hostId = value.text("host_id")
        require(hostId.matches(Regex("[0-9a-f]{64}"))) { "Invalid host identity" }
        val pairId = value.text("pair_id")
        require(pairId.isNotBlank() && pairId.length <= 256) { "Invalid pairing identity" }
        val secret = value.text("secret"); nonce(secret)
        val expires = value.getValue("expires_at").jsonPrimitive.long
        require(expires > nowSeconds() && expires <= nowSeconds() + 330) { "Pairing QR expired or invalid. Generate a new QR on desktop." }
        val relay = value.text("relay_url")
        val pin = fingerprint(value.optionalText("tls_fingerprint"))
        validateRelay(relay, pin)
        return Computer(hostId, value.text("host_name").take(256), relay, pin, "pair:$pairId", secret, expires)
    }
    private fun storageKey(): SecretKey {
        val store = KeyStore.getInstance("AndroidKeyStore").apply { load(null) }
        (store.getKey(KEY_ALIAS, null) as? SecretKey)?.let { return it }
        return KeyGenerator.getInstance(KeyProperties.KEY_ALGORITHM_AES, "AndroidKeyStore").apply {
            init(KeyGenParameterSpec.Builder(KEY_ALIAS, KeyProperties.PURPOSE_ENCRYPT or KeyProperties.PURPOSE_DECRYPT)
                .setBlockModes(KeyProperties.BLOCK_MODE_GCM).setEncryptionPaddings(KeyProperties.ENCRYPTION_PADDING_NONE).build())
        }.generateKey()
    }
    override suspend fun loadComputers(): List<Computer> = withContext(Dispatchers.IO) {
        val prefs = app.getSharedPreferences("paired_computers", Context.MODE_PRIVATE)
        val stored = prefs.getString("encrypted_v1", null) ?: return@withContext emptyList()
        val record = wireJson.parseToJsonElement(stored).jsonObject
        val cipher = Cipher.getInstance("AES/GCM/NoPadding")
        cipher.init(Cipher.DECRYPT_MODE, storageKey(), GCMParameterSpec(128, decode(record.text("iv"))))
        val plain = cipher.doFinal(decode(record.text("data"))).toString(Charsets.UTF_8)
        val storedComputers = wireJson.parseToJsonElement(plain).jsonArray.map { Computer.fromJson(it.jsonObject) }
        val computers = listOfNotNull(retainedComputer(storedComputers))
        if (computers != storedComputers) withContext(NonCancellable) { saveComputers(computers) }
        computers
    }
    override suspend fun saveComputers(computers: List<Computer>) = withContext(Dispatchers.IO) {
        require(computers.size <= 1) { "Only one paired computer can be stored" }
        val plain = JsonArray(computers.map { it.toJson() }).toString().toByteArray(Charsets.UTF_8)
        val cipher = Cipher.getInstance("AES/GCM/NoPadding")
        cipher.init(Cipher.ENCRYPT_MODE, storageKey())
        val record = obj("iv" to JsonPrimitive(encode(cipher.iv)), "data" to JsonPrimitive(encode(cipher.doFinal(plain))))
        check(app.getSharedPreferences("paired_computers", Context.MODE_PRIVATE).edit().putString("encrypted_v1", record.toString()).commit()) { "Could not securely save paired computers" }
    }
    override suspend fun connect(computer: Computer): RemoteSocket = withContext(Dispatchers.IO) {
        validateRelay(computer.relayUrl, computer.tlsFingerprint)
        val builder = OkHttpClient.Builder().pingInterval(25, TimeUnit.SECONDS).connectTimeout(10, TimeUnit.SECONDS).readTimeout(0, TimeUnit.SECONDS)
        if (computer.relayUrl.startsWith("wss:")) {
            val pin = fingerprint(computer.tlsFingerprint)!!
            val trust = object : X509TrustManager {
                override fun getAcceptedIssuers(): Array<X509Certificate> = emptyArray()
                override fun checkClientTrusted(chain: Array<X509Certificate>, authType: String) = throw java.security.cert.CertificateException("Client TLS not supported")
                override fun checkServerTrusted(chain: Array<X509Certificate>, authType: String) {
                    if (chain.isEmpty()) throw java.security.cert.CertificateException("Missing relay certificate")
                    chain[0].checkValidity()
                    if (hex(MessageDigest.getInstance("SHA-256").digest(chain[0].encoded)) != pin) throw java.security.cert.CertificateException("Relay certificate pin mismatch")
                }
            }
            val ssl = SSLContext.getInstance("TLS").apply { init(null, arrayOf(trust), random) }
            builder.sslSocketFactory(ssl.socketFactory, trust)
            // OkHttp's standard hostname verifier still verifies the IP/DNS SAN.
        }
        val client = builder.build()
        val messages = Channel<String>(128)
        suspendCancellableCoroutine { continuation ->
            val webSocket = client.newWebSocket(Request.Builder().url(computer.relayUrl).build(), object : WebSocketListener() {
                override fun onOpen(webSocket: WebSocket, response: Response) {
                    if (continuation.isActive) continuation.resume(object : RemoteSocket {
                        override suspend fun send(message: String) = withContext(Dispatchers.IO) {
                            require(message.toByteArray(Charsets.UTF_8).size <= MAX_FRAME) { "Message too large" }
                            check(webSocket.send(message)) { "Socket closed" }
                        }
                        override suspend fun receive(): String = messages.receive()
                        override fun close() { webSocket.cancel(); messages.cancel(); client.connectionPool.evictAll(); client.dispatcher.executorService.shutdown() }
                    }) else webSocket.cancel()
                }
                override fun onMessage(webSocket: WebSocket, text: String) {
                    if (text.toByteArray(Charsets.UTF_8).size > MAX_FRAME) {
                        messages.close(IllegalStateException("Relay frame limit exceeded")); webSocket.cancel()
                        return
                    }
                    // OkHttp invokes this on its reader thread, never the UI thread.
                    // A bounded inbox applies TCP backpressure during history replay
                    // rather than disconnecting a healthy, slower renderer.
                    if (messages.trySendBlocking(text).isFailure) webSocket.cancel()
                }
                override fun onMessage(webSocket: WebSocket, bytes: ByteString) { messages.close(IllegalStateException("Unexpected binary relay frame")); webSocket.cancel() }
                override fun onClosing(webSocket: WebSocket, code: Int, reason: String) { webSocket.close(code, null); messages.close(IllegalStateException("Relay disconnected")) }
                override fun onClosed(webSocket: WebSocket, code: Int, reason: String) { messages.close(IllegalStateException("Relay disconnected")) }
                override fun onFailure(webSocket: WebSocket, t: Throwable, response: Response?) {
                    messages.close(t)
                    if (continuation.isActive) continuation.resumeWithException(t)
                    client.connectionPool.evictAll(); client.dispatcher.executorService.shutdown()
                }
            })
            continuation.invokeOnCancellation { webSocket.cancel(); messages.cancel(); client.connectionPool.evictAll(); client.dispatcher.executorService.shutdown() }
        }
    }
}
private class AndroidSecureSession(secret: ByteArray, client: ByteArray, host: ByteArray) : SecureSession {
    private val sendKey: ByteArray
    private val receiveKey: ByteArray
    private var sendSeq = 0L
    private var receiveSeq = 0L
    override val verificationCode = hex(MessageDigest.getInstance("SHA-256").digest(client + host)).take(8)
    init {
        val extract = Mac.getInstance("HmacSHA256").apply { init(SecretKeySpec(client + host, "HmacSHA256")) }.doFinal(secret)
        fun expand(info: String) = Mac.getInstance("HmacSHA256").apply { init(SecretKeySpec(extract, "HmacSHA256")) }.doFinal(info.toByteArray(Charsets.UTF_8) + byteArrayOf(1))
        sendKey = expand(C2H); receiveKey = expand(H2C)
        extract.fill(0); secret.fill(0)
    }
    private fun cipher(mode: Int, key: ByteArray, sequence: Long, info: String): Cipher {
        require(sequence >= 0 && sequence < Long.MAX_VALUE) { "Sequence exhausted" }
        return Cipher.getInstance("AES/GCM/NoPadding").apply {
            init(mode, SecretKeySpec(key, "AES"), GCMParameterSpec(128, ByteBuffer.allocate(12).putInt(0).putLong(sequence).array()))
            updateAAD(info.toByteArray(Charsets.UTF_8))
        }
    }
    override fun encrypt(plain: String): JsonObject {
        val bytes = plain.toByteArray(Charsets.UTF_8)
        require(bytes.size <= 700_000) { "Encrypted message too large" }
        val seq = sendSeq
        val encrypted = cipher(Cipher.ENCRYPT_MODE, sendKey, seq, C2H).doFinal(bytes)
        sendSeq++
        return obj("kind" to JsonPrimitive("encrypted"), "seq" to JsonPrimitive(seq), "data" to JsonPrimitive(encode(encrypted)))
    }
    override fun decrypt(envelope: JsonObject): String {
        require(envelope.text("kind") == "encrypted") { "Expected encrypted message" }
        val seq = envelope.getValue("seq").jsonPrimitive.long
        require(seq == receiveSeq) { "Replay or out-of-order encrypted message" }
        val plain = cipher(Cipher.DECRYPT_MODE, receiveKey, seq, H2C).doFinal(decode(envelope.text("data")))
        receiveSeq++
        return plain.toString(Charsets.UTF_8)
    }
}
