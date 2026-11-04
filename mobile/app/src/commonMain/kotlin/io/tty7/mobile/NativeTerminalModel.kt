package io.tty7.mobile

data class NativeTerminalStyle(
    val foreground: Int = -1,
    val background: Int = -2,
    val bold: Boolean = false,
    val dim: Boolean = false,
    val italic: Boolean = false,
    val inverse: Boolean = false,
    val hidden: Boolean = false,
    val strike: Boolean = false,
    val underline: Int = 0,
    val underlineColor: Int? = null,
)

data class NativeTerminalCell(
    val column: Int,
    val width: Int,
    val text: String,
    val style: NativeTerminalStyle,
)

data class NativeTerminalRow(
    val cols: Int,
    val cells: List<NativeTerminalCell>,
    val wrapped: Boolean = false,
    val wrapPadding: Boolean = false,
)

/** Decodes the desktop's positioned cells, not an arbitrary terminal byte stream. */
object NativeTerminalDecoder {
    fun row(ansi: String, cols: Int, wrapped: Boolean = false, wrapPadding: Boolean = false): NativeTerminalRow {
        require(cols in 1..1000) { "Invalid canonical row width" }
        require(!wrapPadding || wrapped) { "Wrap padding without a wrapped row" }
        return RowDecoder(ansi, cols).decode(wrapped, wrapPadding)
    }

    private val defaultStyle = NativeTerminalStyle()

    private class RowDecoder(private val source: String, private val cols: Int) {
        private var offset = 0
        private var foreground = -1
        private var background = -2
        private var bold = false
        private var dim = false
        private var italic = false
        private var inverse = false
        private var hidden = false
        private var strike = false
        private var underline = 0
        private var underlineColor: Int? = null
        private var cachedStyle = defaultStyle
        private var styleChanged = false
        private var column = -1
        private var textStart = -1
        private var textEnd = -1
        private var cellStyle = defaultStyle
        private var retained = 0
        private val cells = ArrayList<NativeTerminalCell>()

        fun decode(wrapped: Boolean, wrapPadding: Boolean): NativeTerminalRow {
            while (offset < source.length) {
                if (source[offset] == '\u001b') {
                    offset++
                    require(offset < source.length && source[offset++] == '[') { "Unsupported canonical escape" }
                    val first = number()
                    if (peek() == 'G') {
                        require(first in 1..cols) { "Canonical column outside grid" }
                        val next = first - 1
                        if (column >= 0) finishCell(next)
                        column = next
                        textStart = -1
                        textEnd = -1
                        offset++
                    } else {
                        sgr(first)
                    }
                } else {
                    require(column >= 0 && textStart < 0) { "Unpositioned or split canonical cell" }
                    textStart = offset
                    cellStyle = style()
                    while (offset < source.length && source[offset] != '\u001b') {
                        val char = source[offset++]
                        require(char.code >= 0x20 && char.code !in 0x7f..0x9f) { "Control in canonical cell" }
                        when (char.code) {
                            in 0xd800..0xdbff -> {
                                require(offset < source.length && source[offset].code in 0xdc00..0xdfff) { "Invalid canonical surrogate" }
                                offset++
                            }
                            in 0xdc00..0xdfff -> throw IllegalArgumentException("Invalid canonical surrogate")
                        }
                    }
                    textEnd = offset
                }
            }
            if (column >= 0) finishCell(cols)
            // Plain trailing padding has no pixels or text to preserve. Wrapped rows
            // still carry their canonical extent in cols (minus wrapPadding).
            if (retained < cells.size) cells.subList(retained, cells.size).clear()
            return NativeTerminalRow(cols, cells, wrapped, wrapPadding)
        }

        private fun finishCell(next: Int) {
            val width = next - column
            require(width in 1..2) { "Invalid canonical cell width or column order" }
            require(textStart >= 0 && textEnd > textStart) { "Empty positioned canonical cell" }
            val text = if (textEnd == textStart + 1 && source[textStart] == ' ') " "
                else source.substring(textStart, textEnd)
            cells.add(NativeTerminalCell(column, width, text, cellStyle))
            if (text != " " || cellStyle.background != -2 || cellStyle.inverse ||
                cellStyle.underline != 0 || cellStyle.strike) retained = cells.size
        }

        private fun peek(): Char {
            require(offset < source.length) { "Truncated canonical escape" }
            return source[offset]
        }

        /** Empty parameters are -1; no substring or parameter-list allocation. */
        private fun number(): Int {
            var value = -1
            while (offset < source.length && source[offset] in '0'..'9') {
                val digit = source[offset++].code - '0'.code
                if (value < 0) value = 0
                require(value <= (Int.MAX_VALUE - digit) / 10) { "Canonical parameter overflow" }
                value = value * 10 + digit
            }
            return value
        }

        private fun parameter(separator: Char, allowEmpty: Boolean = false): Int {
            require(peek() == separator) { "Malformed canonical SGR" }
            offset++
            val value = number()
            require(allowEmpty || value >= 0) { "Missing canonical SGR parameter" }
            return value
        }

        private fun channel(value: Int): Int {
            require(value in 0..255) { "Invalid canonical color component" }
            return value
        }

        private fun color(colon: Boolean): Int {
            val separator = if (colon) ':' else ';'
            return when (parameter(separator)) {
                5 -> channel(parameter(separator))
                2 -> {
                    var red = parameter(separator, allowEmpty = colon)
                    var green = parameter(separator)
                    var blue = parameter(separator)
                    if (colon && peek() == ':') {
                        // ISO color-space slot: omitted or RGB (0), never reinterpreted.
                        require(red == -1 || red == 0) { "Unsupported canonical color space" }
                        red = green
                        green = blue
                        blue = parameter(':')
                    }
                    0x1000000 or (channel(red) shl 16) or (channel(green) shl 8) or channel(blue)
                }
                else -> throw IllegalArgumentException("Unsupported canonical color mode")
            }
        }

        private fun sgr(first: Int) {
            var code = first
            while (true) {
                if (peek() == ':') {
                    when (code) {
                        4 -> underline = parameter(':').also {
                            require(it in 0..5) { "Unsupported canonical underline" }
                        }
                        38 -> foreground = color(colon = true)
                        48 -> background = color(colon = true)
                        58 -> underlineColor = color(colon = true)
                        else -> throw IllegalArgumentException("Unsupported canonical SGR subparameters")
                    }
                    styleChanged = true
                } else {
                    when (if (code < 0) 0 else code) {
                        0 -> resetStyle()
                        1 -> { bold = true; styleChanged = true }
                        2 -> { dim = true; styleChanged = true }
                        3 -> { italic = true; styleChanged = true }
                        4 -> { underline = 1; styleChanged = true }
                        7 -> { inverse = true; styleChanged = true }
                        8 -> { hidden = true; styleChanged = true }
                        9 -> { strike = true; styleChanged = true }
                        21 -> { underline = 2; styleChanged = true }
                        22 -> { bold = false; dim = false; styleChanged = true }
                        23 -> { italic = false; styleChanged = true }
                        24 -> { underline = 0; styleChanged = true }
                        27 -> { inverse = false; styleChanged = true }
                        28 -> { hidden = false; styleChanged = true }
                        29 -> { strike = false; styleChanged = true }
                        in 30..37 -> { foreground = code - 30; styleChanged = true }
                        38 -> { foreground = color(colon = false); styleChanged = true }
                        39 -> { foreground = -1; styleChanged = true }
                        in 40..47 -> { background = code - 40; styleChanged = true }
                        48 -> { background = color(colon = false); styleChanged = true }
                        49 -> { background = -2; styleChanged = true }
                        58 -> { underlineColor = color(colon = false); styleChanged = true }
                        59 -> { underlineColor = null; styleChanged = true }
                        in 90..97 -> { foreground = code - 90 + 8; styleChanged = true }
                        in 100..107 -> { background = code - 100 + 8; styleChanged = true }
                        else -> throw IllegalArgumentException("Unsupported canonical SGR")
                    }
                }
                when (peek()) {
                    'm' -> { offset++; return }
                    ';' -> { offset++; code = number() }
                    else -> throw IllegalArgumentException("Unsupported canonical control")
                }
            }
        }

        private fun resetStyle() {
            foreground = -1
            background = -2
            bold = false
            dim = false
            italic = false
            inverse = false
            hidden = false
            strike = false
            underline = 0
            underlineColor = null
            cachedStyle = defaultStyle
            styleChanged = false
        }

        private fun style(): NativeTerminalStyle {
            if (styleChanged) {
                cachedStyle = NativeTerminalStyle(foreground, background, bold, dim, italic,
                    inverse, hidden, strike, underline, underlineColor)
                styleChanged = false
            }
            return cachedStyle
        }
    }
}

/** Source RGB is never adjusted for contrast, including Nerd/Powerline glyphs. */
class NativeTerminalColors(val dark: Boolean) {
    val foreground: Int = opaque(if (dark) 0xececed else 0x1c1c1e)
    val background: Int = opaque(if (dark) 0x1e1e20 else 0xfcfcfb)
    private val palette = if (dark) darkPalette else lightPalette

    fun resolve(token: Int): Int = when {
        token == -1 -> foreground
        token == -2 -> background
        token in 0..15 -> opaque(palette[token])
        token in 16..231 -> {
            val index = token - 16
            opaque((cube(index / 36) shl 16) or (cube(index / 6 % 6) shl 8) or cube(index % 6))
        }
        token in 232..255 -> {
            val gray = 8 + (token - 232) * 10
            opaque((gray shl 16) or (gray shl 8) or gray)
        }
        token in 0x1000000..0x1ffffff -> opaque(token and 0xffffff)
        else -> throw IllegalArgumentException("Invalid canonical color token")
    }

    private companion object {
        fun opaque(rgb: Int): Int = rgb or 0xff000000.toInt()
        fun cube(value: Int): Int = if (value == 0) 0 else 55 + value * 40
        val lightPalette = intArrayOf(0x24292e, 0xd1242f, 0x1a7f37, 0x9a6700, 0x0969da, 0x8250df, 0x1b7c83, 0x6e7781,
            0x57606a, 0xcf222e, 0x1f883d, 0xbf8700, 0x218bff, 0xa475f9, 0x3192aa, 0x8c959f)
        val darkPalette = intArrayOf(0x616161, 0xff8272, 0xb4fa72, 0xfefdc2, 0xa5d5fe, 0xff8ffd, 0xd0d1fe, 0xf1f1f1,
            0x8e8e8e, 0xffc4bd, 0xd6fcb9, 0xfefdd5, 0xc1e3fe, 0xffb1fe, 0xe5e6fe, 0xfeffff)
    }
}
