package io.tty7.mobile

import android.Manifest
import android.annotation.SuppressLint
import android.content.pm.PackageManager
import androidx.activity.compose.rememberLauncherForActivityResult
import androidx.activity.result.contract.ActivityResultContracts
import androidx.camera.core.CameraSelector
import androidx.camera.core.ImageAnalysis
import androidx.camera.core.Preview
import androidx.camera.lifecycle.ProcessCameraProvider
import androidx.camera.view.PreviewView
import androidx.compose.foundation.layout.*
import androidx.compose.material3.*
import androidx.compose.runtime.*
import androidx.compose.ui.Modifier
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.platform.testTag
import androidx.compose.ui.unit.dp
import androidx.compose.ui.viewinterop.AndroidView
import androidx.core.content.ContextCompat
import androidx.lifecycle.compose.LocalLifecycleOwner
import com.google.mlkit.vision.barcode.BarcodeScanning
import com.google.mlkit.vision.barcode.common.Barcode
import com.google.mlkit.vision.barcode.BarcodeScannerOptions
import com.google.mlkit.vision.common.InputImage
import java.util.concurrent.Executors
import java.util.concurrent.atomic.AtomicBoolean

@SuppressLint("UnsafeOptInUsageError")
@Composable
actual fun CameraScanner(onScan: (String) -> Unit, onClose: () -> Unit) {
    val context = LocalContext.current
    val lifecycle = LocalLifecycleOwner.current
    val currentScan by rememberUpdatedState(onScan)
    var granted by remember { mutableStateOf(ContextCompat.checkSelfPermission(context, Manifest.permission.CAMERA) == PackageManager.PERMISSION_GRANTED) }
    var asked by remember { mutableStateOf(false) }
    var error by remember { mutableStateOf<String?>(null) }
    val launcher = rememberLauncherForActivityResult(ActivityResultContracts.RequestPermission()) { granted = it; asked = true }
    LaunchedEffect(Unit) { if (!granted) launcher.launch(Manifest.permission.CAMERA) }
    val previewView = remember { PreviewView(context).apply { contentDescription = "二维码扫描相机预览"; scaleType = PreviewView.ScaleType.FILL_CENTER } }
    Column(Modifier.fillMaxSize().safeDrawingPadding().padding(12.dp)) {
        Text("扫描电脑上的配对二维码", style = MaterialTheme.typography.titleLarge)
        Text("请打开电脑上 tty7 的「配对设备」设置，将二维码对准相机。", style = MaterialTheme.typography.bodyMedium)
        TextButton(onClick = onClose, modifier = Modifier.testTag("close-scanner")) { Text("取消") }
        error?.let { Text(it, color = MaterialTheme.colorScheme.error) }
        if (granted) AndroidView(factory = { previewView }, modifier = Modifier.fillMaxWidth().weight(1f).testTag("camera-preview"))
        else {
            Text(if (asked) "相机权限被拒绝。请在 Android 设置中允许访问相机，或返回后粘贴配对链接。" else "扫描二维码需要相机权限。")
            Button(onClick = { launcher.launch(Manifest.permission.CAMERA) }) { Text("允许访问相机") }
        }
    }
    DisposableEffect(granted, lifecycle, previewView) {
        val executor = Executors.newSingleThreadExecutor()
        val disposed = AtomicBoolean(false)
        val found = AtomicBoolean(false)
        val scanner = BarcodeScanning.getClient(BarcodeScannerOptions.Builder().setBarcodeFormats(Barcode.FORMAT_QR_CODE).build())
        var provider: ProcessCameraProvider? = null
        var analysis: ImageAnalysis? = null
        var preview: Preview? = null
        if (granted) {
            val future = ProcessCameraProvider.getInstance(context)
            future.addListener({
                if (!disposed.get()) try {
                    provider = future.get()
                    preview = Preview.Builder().build().also { it.setSurfaceProvider(previewView.surfaceProvider) }
                    analysis = ImageAnalysis.Builder().setBackpressureStrategy(ImageAnalysis.STRATEGY_KEEP_ONLY_LATEST).build().also { stream ->
                        stream.setAnalyzer(executor) { image ->
                            val media = image.image
                            if (media == null || disposed.get() || found.get()) image.close()
                            else scanner.process(InputImage.fromMediaImage(media, image.imageInfo.rotationDegrees))
                                .addOnSuccessListener { barcodes ->
                                    val uri = barcodes.firstOrNull { it.rawValue?.startsWith("tty7://pair?") == true }?.rawValue
                                    if (uri != null && !disposed.get() && found.compareAndSet(false, true)) currentScan(uri)
                                }
                                .addOnFailureListener { if (!disposed.get()) error = "二维码扫描失败：${it.message ?: "相机错误"}" }
                                .addOnCompleteListener { image.close() }
                        }
                    }
                    provider!!.bindToLifecycle(lifecycle, CameraSelector.DEFAULT_BACK_CAMERA, preview!!, analysis!!)
                } catch (e: Exception) { error = "无法打开相机：${e.message ?: "相机不可用"}" }
            }, ContextCompat.getMainExecutor(context))
        }
        onDispose {
            disposed.set(true)
            analysis?.clearAnalyzer()
            analysis?.let { provider?.unbind(it) }
            preview?.let { provider?.unbind(it) }
            scanner.close()
            executor.shutdown()
        }
    }
}
