package io.tty7.mobile

import android.graphics.Bitmap
import android.graphics.Canvas
import android.graphics.Paint
import android.graphics.Typeface
import android.view.InputDevice
import android.view.MotionEvent
import android.view.KeyEvent
import android.view.inputmethod.EditorInfo
import androidx.test.platform.app.InstrumentationRegistry
import kotlinx.serialization.json.JsonPrimitive
import kotlinx.serialization.json.JsonArray
import org.junit.Test
import kotlin.test.assertEquals
import kotlin.test.assertFalse
import kotlin.test.assertNotNull
import kotlin.test.assertTrue
import kotlin.math.ceil
import kotlin.math.floor
import kotlin.math.max

class NativeTerminalViewTest {
    private fun onMain(block: () -> Unit) = InstrumentationRegistry.getInstrumentation().runOnMainSync(block)
    private fun frame(cols: Int = 2, alternate: Boolean = false) = TerminalFrame(
        "1", null, "1", cols, 1, alternate,
        obj("row" to JsonPrimitive(0), "col" to JsonPrimitive(0), "shape" to JsonPrimitive("block"),
            "visible" to JsonPrimitive(false), "blink" to JsonPrimitive(false)),
        obj("app_cursor" to JsonPrimitive(true), "app_keypad" to JsonPrimitive(false),
            "bracketed_paste" to JsonPrimitive(true), "mouse" to JsonPrimitive(false),
            "alternate_scroll" to JsonPrimitive(true), "sgr_mouse" to JsonPrimitive(false)),
        emptyList(), "test", 1uL, 0, true,
    )

    @Test fun chinesePreeditStaysLocalAndCommitAndDeleteAreSentExactlyOnce() = onMain {
        val view = NativeTerminalView(InstrumentationRegistry.getInstrumentation().targetContext)
        val sent = mutableListOf<String>()
        view.onInput = { sent.add(it) }
        view.setInputEnabled(true)
        val input = assertNotNull(view.onCreateInputConnection(EditorInfo()))
        input.setComposingText("中", 1)
        input.setComposingText("中文", 1)
        assertEquals(emptyList(), sent)
        input.commitText("中文", 1)
        input.finishComposingText()
        assertEquals(listOf("中文"), sent)
        input.deleteSurroundingText(1, 0)
        assertEquals(listOf("中文", "\u007f"), sent)
        view.setInputEnabled(false)
        input.commitText("不能发送", 1)
        assertEquals(listOf("中文", "\u007f"), sent)
        view.dispose()
    }

    @Test fun applicationCursorKeysAndEnterRespectTheRemoteInputMode() = onMain {
        val view = NativeTerminalView(InstrumentationRegistry.getInstrumentation().targetContext)
        val sent = mutableListOf<String>()
        view.onInput = { sent.add(it) }
        view.applyScreen(frame(), listOf(0 to NativeTerminalDecoder.row("", 2)))
        view.setInputEnabled(true)
        val input = assertNotNull(view.onCreateInputConnection(EditorInfo()))
        input.sendKeyEvent(KeyEvent(KeyEvent.ACTION_DOWN, KeyEvent.KEYCODE_DPAD_UP))
        input.sendKeyEvent(KeyEvent(KeyEvent.ACTION_UP, KeyEvent.KEYCODE_DPAD_UP))
        input.sendKeyEvent(KeyEvent(KeyEvent.ACTION_DOWN, KeyEvent.KEYCODE_ENTER))
        input.sendKeyEvent(KeyEvent(KeyEvent.ACTION_UP, KeyEvent.KEYCODE_ENTER))
        assertEquals(listOf("\u001bOA", "\r"), sent)
        view.dispose()
    }

    @Test fun staleImeConnectionCannotTypeIntoAnotherPane() = onMain {
        val view = NativeTerminalView(InstrumentationRegistry.getInstrumentation().targetContext)
        val sent = mutableListOf<String>()
        view.onInput = { sent.add(it) }
        view.setInputEnabled(true)
        val oldInput = assertNotNull(view.onCreateInputConnection(EditorInfo()))
        oldInput.setComposingText("旧会话", 1)
        view.reset()
        view.setInputEnabled(true)
        val newInput = assertNotNull(view.onCreateInputConnection(EditorInfo()))
        oldInput.commitText("旧会话", 1)
        oldInput.finishComposingText()
        oldInput.deleteSurroundingText(1, 0)
        assertEquals(emptyList(), sent)
        newInput.commitText("新会话", 1)
        assertEquals(listOf("新会话"), sent)
        view.dispose()
    }

    @Test fun powerlinePixelsKeepSourceRgbAcrossThemeChanges() = onMain {
        val view = NativeTerminalView(InstrumentationRegistry.getInstrumentation().targetContext)
        view.layout(0, 0, 320, 240)
        val row = NativeTerminalDecoder.row(
            "\u001b[1G\u001b[38;2;118;159;240;48;2;57;66;96m\ue0b4\u001b[2G \u001b[0m", 2)
        view.applyScreen(frame(), listOf(0 to row))
        val bitmap = Bitmap.createBitmap(320, 240, Bitmap.Config.ARGB_8888)
        val pixels = IntArray(320 * 240)
        for (dark in listOf(false, true)) {
            view.setAppearance(dark)
            view.draw(Canvas(bitmap))
            bitmap.getPixels(pixels, 0, 320, 0, 0, 320, 240)
            assertTrue(pixels.any { it == 0xff769ff0.toInt() }, "Powerline fill must retain the original blue")
            assertTrue(pixels.any { it == 0xff394260.toInt() }, "The adjacent segment must retain its background")
            assertFalse(pixels.any { it == 0xffffffff.toInt() }, "Contrast correction must not create a white separator")
            assertFalse((0 until 320 * 120).any { pixels[it] == 0xff769ff0.toInt() },
                "Short shell output stays at the bottom rather than moving the prompt above unused space")
        }
        bitmap.recycle()
        view.dispose()
    }

    @Test fun embeddedTerminalDoesNotPaintOverItsParentChrome() = onMain {
        val view = NativeTerminalView(InstrumentationRegistry.getInstrumentation().targetContext)
        view.layout(0, 0, 160, 120)
        view.setAppearance(false)
        val bitmap = Bitmap.createBitmap(320, 320, Bitmap.Config.ARGB_8888)
        bitmap.eraseColor(0xffcc22aa.toInt())
        val canvas = Canvas(bitmap)
        canvas.translate(64f, 64f)
        view.draw(canvas)
        assertEquals(0xffcc22aa.toInt(), bitmap.getPixel(0, 0),
            "A translated AndroidView must not erase the Compose application bar")
        assertEquals(0xffcc22aa.toInt(), bitmap.getPixel(300, 300),
            "Native terminal background must remain inside the allocated view bounds")
        assertEquals(NativeTerminalColors(false).background, bitmap.getPixel(100, 170))
        bitmap.recycle()
        view.dispose()
    }
    @Test fun measuredGridUsesRealFontCellsAndViewportWithoutChromeAndUpdatesOnResize() = onMain {
        val context = InstrumentationRegistry.getInstrumentation().targetContext
        val view = NativeTerminalView(context)
        val measured = mutableListOf<Pair<Int, Int>>()
        view.onResize = { cols, rows -> measured.add(cols to rows) }
        view.layout(0, 0, 720, 1000)
        val paint = Paint().apply {
            @Suppress("DEPRECATION")
            textSize = 14f * context.resources.displayMetrics.scaledDensity
            typeface = Typeface.createFromAsset(context.assets, "terminal/fonts/JetBrainsMonoNerdFontMono-Regular.ttf")
        }
        val density = context.resources.displayMetrics.density
        val metrics = paint.fontMetrics
        val cellHeight = ceil(max(paint.textSize * 1.3f, metrics.descent - metrics.ascent))
        val expected = floor((720 - 16f * density) / paint.measureText("M")).toInt().coerceIn(20, 1000) to
            floor((1000 - 36f * density) / cellHeight).toInt().coerceIn(5, 1000)
        assertEquals(listOf(expected), measured)
        view.reportGridSize()
        view.layout(0, 0, 720, 1000)
        assertEquals(1, measured.size, "Repeated layout/composition must not resend the same geometry")
        view.layout(0, 0, 720, 500)
        assertTrue(measured.last().second < expected.second, "IME-reduced height must resize the PTY")
        view.layout(0, 0, 1000, 720)
        assertTrue(measured.last().first > expected.first, "Rotation must use the new available width")
        val count = measured.size
        view.reset()
        view.reportGridSize()
        assertEquals(count + 1, measured.size, "A new feed must receive an initial measurement even at the same size")
        view.dispose()
    }

    @Test fun normalScreenAndOlderHistoryKeepPhysicalRowsAndCanPanHorizontally() = onMain {
        val context = InstrumentationRegistry.getInstrumentation().targetContext
        val view = NativeTerminalView(context)
        val width = 640
        val height = 900
        view.layout(0, 0, width, height)
        val red = 0xffdc212c.toInt()
        val cyan = 0xff19d9da.toInt()
        val blue = 0xff2147dc.toInt()
        fun canonical(text: String) = buildString {
            text.forEachIndexed { column, char -> append("\u001b[${column + 1}G").append(char) }
        }
        val ansi = "\u001b[48;2;220;33;44m" + canonical("─".repeat(119)) +
            "\u001b[120G\u001b[48;2;25;217;218m│"
        val rows = listOf(NativeTerminalDecoder.row(ansi, 120, wrapped = true),
            NativeTerminalDecoder.row("\u001b[48;2;33;71;220m" + canonical("─".repeat(120)), 120))
        view.applyScreen(frame(cols = 120).copy(rows = 2), rows.mapIndexed { index, row -> index to row })
        view.setInputEnabled(true)
        val bitmap = Bitmap.createBitmap(width, height, Bitmap.Config.ARGB_8888)
        val pixels = IntArray(width * height)
        fun snapshot() {
            view.draw(Canvas(bitmap))
            bitmap.getPixels(pixels, 0, width, 0, 0, width, height)
        }
        fun assertPhysicalRows() {
            val redYs = pixels.indices.filter { pixels[it] == red }.map { it / width }
            val blueYs = pixels.indices.filter { pixels[it] == blue }.map { it / width }
            assertTrue(redYs.isNotEmpty(), "The first physical TUI row must stay visible")
            assertTrue(blueYs.isNotEmpty())
            assertTrue(redYs.max() < blueYs.min(), "Wrapped metadata must not merge two physical grid rows")
            @Suppress("DEPRECATION")
            val textSize = 14f * context.resources.displayMetrics.scaledDensity
            val paint = Paint().apply {
                this.textSize = textSize
                typeface = Typeface.createFromAsset(context.assets, "terminal/fonts/JetBrainsMonoNerdFontMono-Regular.ttf")
            }
            val metrics = paint.fontMetrics
            val maximumCellHeight = ceil(max(textSize * 1.3f, metrics.descent - metrics.ascent)).toInt() + 1
            assertTrue(redYs.max() - redYs.min() < maximumCellHeight,
                "A wide TUI row must not be broken into multiple phone-width rows")
        }
        fun panToRight() {
            val pointer = MotionEvent.PointerProperties().apply { id = 0; toolType = MotionEvent.TOOL_TYPE_MOUSE }
            val coords = MotionEvent.PointerCoords().apply {
                x = width / 2f
                y = height / 2f
                setAxisValue(MotionEvent.AXIS_HSCROLL, -1000f)
            }
            val event = MotionEvent.obtain(0, 0, MotionEvent.ACTION_SCROLL, 1, arrayOf(pointer), arrayOf(coords),
                0, 0, 1f, 1f, 0, 0, InputDevice.SOURCE_MOUSE, 0)
            view.onGenericMotionEvent(event)
            event.recycle()
        }
        snapshot()
        assertPhysicalRows()
        assertFalse(pixels.any { it == cyan }, "The right edge starts outside the narrow viewport")
        panToRight()
        snapshot()
        assertTrue(pixels.any { it == cyan }, "Ordinary screen panning must reveal the original rightmost column")
        var historyRequested = false
        view.onHistory = { historyRequested = true }
        val density = context.resources.displayMetrics.density
        val down = MotionEvent.obtain(0, 0, MotionEvent.ACTION_DOWN, 20f * density, 18f * density, 0)
        val up = MotionEvent.obtain(0, 1, MotionEvent.ACTION_UP, 20f * density, 18f * density, 0)
        view.onTouchEvent(down)
        view.onTouchEvent(up)
        down.recycle()
        up.recycle()
        assertTrue(historyRequested)
        val history = HistoryPage(JsonArray(rows.map { obj("cols" to JsonPrimitive(it.cols),
            "ansi" to JsonPrimitive(ansi), "text" to JsonPrimitive("wide history")) }), null, false, false)
        view.applyHistory(history, rows, listOf(ansi.length, ansi.length))
        // A narrower current PTY must not reflow rows captured at the old 120-column width.
        view.applyScreen(frame(cols = 40), listOf(0 to NativeTerminalDecoder.row(canonical("new narrow live".padEnd(40)), 40)))
        snapshot()
        assertPhysicalRows()
        assertFalse(pixels.any { it == cyan })
        panToRight()
        snapshot()
        assertTrue(pixels.any { it == cyan }, "History panning must retain its source width after a live resize")
        bitmap.recycle()
        view.dispose()
    }
}
