package io.tty7.mobile

import kotlin.test.Test
import kotlin.test.assertEquals
import kotlin.test.assertFailsWith
import kotlin.test.assertFalse
import kotlin.test.assertTrue

class NativeTerminalModelTest {
    @Test fun powerlineSourceColorsAreNeverReplacedForContrast() {
        val row = NativeTerminalDecoder.row(
            "\u001b[0m\u001b[1G\u001b[38;2;118;159;240;48;2;57;66;96m\ue0b0" +
                "\u001b[2G\u001b[38;2;57;66;96;48;2;118;159;240m\ue0b2" +
                "\u001b[3G\u001b[0mx\u001b[4G \u001b[0m", 4,
        )
        assertEquals(3, row.cells.size)
        assertEquals(0x1769ff0, row.cells[0].style.foreground)
        assertEquals(0x1394260, row.cells[0].style.background)
        for (dark in listOf(false, true)) {
            val colors = NativeTerminalColors(dark)
            assertEquals(0xff769ff0.toInt(), colors.resolve(row.cells[0].style.foreground))
            assertEquals(0xff394260.toInt(), colors.resolve(row.cells[0].style.background))
            assertEquals(0xff394260.toInt(), colors.resolve(row.cells[1].style.foreground))
            assertEquals(0xff769ff0.toInt(), colors.resolve(row.cells[1].style.background))
        }
    }

    @Test fun sourcePositionsDetermineWidthsAndRetainAtomicUnicodeText() {
        val row = NativeTerminalDecoder.row(
            "\u001b[0m\u001b[1G界\u001b[3Ge\u0301\u001b[4G❤\ufe0f" +
                "\u001b[5G\ud83d\udc69\u200d\ud83d\udcbb\u001b[7G\udb80\udc01\u001b[8G \u001b[0m", 8,
        )
        assertEquals(listOf(0, 2, 3, 4, 6), row.cells.map { it.column })
        assertEquals(listOf(2, 1, 1, 2, 1), row.cells.map { it.width })
        assertEquals(listOf("界", "e\u0301", "❤\ufe0f", "\ud83d\udc69\u200d\ud83d\udcbb", "\udb80\udc01"),
            row.cells.map { it.text })
        // There is no following CHA at the right edge; the canonical extent supplies it.
        assertEquals(2, NativeTerminalDecoder.row("\u001b[1G界\u001b[0m", 2).cells.single().width)
    }

    @Test fun flagsInverseAndSelectiveDefaultsAreRetainedWithoutApplyingColors() {
        val first = NativeTerminalStyle(foreground = 1, background = 12, bold = true, dim = true,
            italic = true, inverse = true, hidden = true, strike = true, underline = 3,
            underlineColor = 0x1010203)
        val row = NativeTerminalDecoder.row(
            "\u001b[1G\u001b[31;104;1;2;3;7;8;9;4:3;58;2;1;2;3mX" +
                "\u001b[2G\u001b[22;23;27;28;29;24;59;39;49mY\u001b[0m", 2,
        )
        assertEquals(first, row.cells[0].style)
        assertEquals(NativeTerminalStyle(), row.cells[1].style)
        val reset = NativeTerminalDecoder.row("\u001b[1G\u001b[1;31;48;5;200;7mX\u001b[2G\u001b[mY", 2)
        assertEquals(NativeTerminalStyle(), reset.cells[1].style)
        assertTrue(reset.cells[0].style.inverse)
        assertEquals(1, reset.cells[0].style.foreground)
        assertEquals(200, reset.cells[0].style.background)
    }

    @Test fun underlineVariantsAndColonColorFormsMatchCanonicalStyles() {
        for (underline in 0..5) {
            val row = NativeTerminalDecoder.row("\u001b[1G\u001b[4:${underline}mX\u001b[0m", 1)
            assertEquals(underline, row.cells.single().style.underline)
        }
        assertEquals(2, NativeTerminalDecoder.row("\u001b[1G\u001b[21mX", 1).cells.single().style.underline)
        assertEquals(1, NativeTerminalDecoder.row("\u001b[1G\u001b[4mX", 1).cells.single().style.underline)
        val row = NativeTerminalDecoder.row(
            "\u001b[1G\u001b[38:2::118:159:240;48:2:0:57:66:96;58:5:196;4:5mX" +
                "\u001b[2G\u001b[38:2:1:2:3;48:5:42;59mY\u001b[0m", 2,
        )
        assertEquals(0x1769ff0, row.cells[0].style.foreground)
        assertEquals(0x1394260, row.cells[0].style.background)
        assertEquals(196, row.cells[0].style.underlineColor)
        assertEquals(5, row.cells[0].style.underline)
        assertEquals(0x1010203, row.cells[1].style.foreground)
        assertEquals(42, row.cells[1].style.background)
        assertEquals(null, row.cells[1].style.underlineColor)
    }

    @Test fun emptyRowsAndTrailingPaddingKeepOnlyVisibleOrDecoratedBlanks() {
        assertTrue(NativeTerminalDecoder.row("", 3).cells.isEmpty())
        assertTrue(NativeTerminalDecoder.row("\u001b[0m\u001b[0m", 3).cells.isEmpty())
        assertTrue(NativeTerminalDecoder.row("\u001b[1G \u001b[2G \u001b[3G \u001b[0m", 3).cells.isEmpty())
        val styled = NativeTerminalDecoder.row(
            "\u001b[1Gx\u001b[2G\u001b[48;2;57;66;96m \u001b[3G\u001b[0m \u001b[0m", 3,
        )
        assertEquals(2, styled.cells.size)
        assertEquals(" ", styled.cells.last().text)
        assertEquals(0x1394260, styled.cells.last().style.background)
        for (sgr in listOf("7", "4", "9")) {
            assertEquals(1, NativeTerminalDecoder.row("\u001b[1G\u001b[${sgr}m ", 1).cells.size)
        }
        // Foreground/bold alone do not paint a trailing space.
        assertTrue(NativeTerminalDecoder.row("\u001b[1G\u001b[1;31m ", 1).cells.isEmpty())
        val wrapped = NativeTerminalDecoder.row("\u001b[1Gx\u001b[2G \u001b[3G ", 3,
            wrapped = true, wrapPadding = true)
        assertEquals(3, wrapped.cols)
        assertEquals(1, wrapped.cells.size)
        assertTrue(wrapped.wrapped)
        assertTrue(wrapped.wrapPadding)
        assertFalse(styled.wrapped)
        assertFalse(styled.wrapPadding)
    }

    @Test fun malformedControlsPositionsAndWidthsCannotCorruptCanonicalRows() {
        val invalid = listOf(
            "x", "\u001b", "\u001b[", "\u001b[1", "\u001b[1G", "\u001b[0Gx", "\u001b[Gx", "\u001b[5Gx",
            "\u001b[-1Gx", "\u001b[1;2Gx", "\u001b[999999999999999999999Gx",
            "\u001b[2Gx\u001b[1Gy", "\u001b[1Gx\u001b[1Gy", "\u001b[1Gx\u001b[4Gy", "\u001b[1Gx",
            "\u001b[1G\u001b[2Gx", "\u001b[1Gx\u001b[31my\u001b[2Gx\u001b[3Gx\u001b[4Gx",
            "\u001b[1G\n", "\u001b[1G\r", "\u001b[1G\t", "\u001b[1G\u007f", "\u001b[1G\u009b",
            "\u001b[1G\ud800", "\u001b[1G\udc00", "\u001b[1G\ud800x", "\u001b]52;c;eA==\u0007",
            "\u001b[2J", "\u001b[?25h", "\u001b[5m", "\u001b[4:6m", "\u001b[1:2m", "\u001b[38;7;1m",
            "\u001b[38;5;256m", "\u001b[48;2;1;2m", "\u001b[58;2;1;2;999m", "\u001b[38:2::1:2m",
            "\u001b[38:2:1:1:2:3m", "\u001b[48:5:42:1m", "\u001b[38;;2m",
        )
        for (ansi in invalid) {
            assertFailsWith<IllegalArgumentException>("Accepted malformed canonical row: $ansi") {
                NativeTerminalDecoder.row(ansi, 4)
            }
        }
        assertFailsWith<IllegalArgumentException> { NativeTerminalDecoder.row("", 0) }
        assertFailsWith<IllegalArgumentException> { NativeTerminalDecoder.row("", 1001) }
        assertFailsWith<IllegalArgumentException> { NativeTerminalDecoder.row("", 4, wrapPadding = true) }
    }

    @Test fun palettesMatchTerminalThemesAndStandardXtermCube() {
        val light = NativeTerminalColors(false)
        val dark = NativeTerminalColors(true)
        assertEquals(0xff1c1c1e.toInt(), light.resolve(-1))
        assertEquals(0xfffcfcfb.toInt(), light.resolve(-2))
        assertEquals(0xffececed.toInt(), dark.resolve(-1))
        assertEquals(0xff1e1e20.toInt(), dark.resolve(-2))
        assertEquals(0xff24292e.toInt(), light.resolve(0))
        assertEquals(0xff616161.toInt(), dark.resolve(0))
        assertEquals(0xff8c959f.toInt(), light.resolve(15))
        assertEquals(0xfffeffff.toInt(), dark.resolve(15))
        for (colors in listOf(light, dark)) {
            assertEquals(0xff000000.toInt(), colors.resolve(16))
            assertEquals(0xff00005f.toInt(), colors.resolve(17))
            assertEquals(0xff5f8700.toInt(), colors.resolve(64))
            assertEquals(0xffffffff.toInt(), colors.resolve(231))
            assertEquals(0xff080808.toInt(), colors.resolve(232))
            assertEquals(0xffeeeeee.toInt(), colors.resolve(255))
            assertEquals(0xff000000.toInt(), colors.resolve(0x1000000))
            assertEquals(0xffffffff.toInt(), colors.resolve(0x1ffffff))
            assertFailsWith<IllegalArgumentException> { colors.resolve(256) }
            assertFailsWith<IllegalArgumentException> { colors.resolve(-3) }
        }
    }
}
