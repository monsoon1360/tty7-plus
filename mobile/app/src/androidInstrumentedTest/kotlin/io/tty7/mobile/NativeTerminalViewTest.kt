package io.tty7.mobile

import android.graphics.Bitmap
import android.graphics.Canvas
import android.view.KeyEvent
import android.view.inputmethod.EditorInfo
import androidx.test.platform.app.InstrumentationRegistry
import kotlinx.serialization.json.JsonPrimitive
import org.junit.Test
import kotlin.test.assertEquals
import kotlin.test.assertFalse
import kotlin.test.assertNotNull
import kotlin.test.assertTrue

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
}
