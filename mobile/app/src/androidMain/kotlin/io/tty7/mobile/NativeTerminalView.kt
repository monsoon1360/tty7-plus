package io.tty7.mobile

import android.content.ClipData
import android.content.ClipboardManager
import android.content.Context
import android.content.res.Configuration
import android.graphics.Canvas
import android.graphics.Paint
import android.graphics.Path
import android.graphics.Rect
import android.graphics.RectF
import android.graphics.Typeface
import android.graphics.fonts.Font
import android.graphics.fonts.FontFamily
import android.graphics.fonts.FontStyle
import android.os.Build
import android.os.Bundle
import android.text.Editable
import android.text.InputType
import android.text.Selection
import android.view.ActionMode
import android.view.KeyCharacterMap
import android.view.KeyEvent
import android.view.Menu
import android.view.MenuItem
import android.view.MotionEvent
import android.view.VelocityTracker
import android.view.View
import android.view.ViewConfiguration
import android.view.accessibility.AccessibilityEvent
import android.view.accessibility.AccessibilityNodeInfo
import android.view.inputmethod.BaseInputConnection
import android.view.inputmethod.EditorInfo
import android.view.inputmethod.InputConnection
import android.view.inputmethod.InputMethodManager
import android.widget.OverScroller
import kotlinx.serialization.json.JsonObject
import kotlinx.serialization.json.booleanOrNull
import kotlinx.serialization.json.intOrNull
import kotlinx.serialization.json.jsonPrimitive
import java.util.IdentityHashMap
import kotlin.math.abs
import kotlin.math.ceil
import kotlin.math.floor
import kotlin.math.max
import kotlin.math.min

/** A projection of the desktop's canonical cells, deliberately not a terminal emulator. */
class NativeTerminalView(context: Context) : View(context) {
    var onInput: (String) -> Unit = {}
    var onHistory: (String?) -> Unit = {}

    private var colors = NativeTerminalColors(true)
    private var darkAppearance = true
    private val fallbackFonts = arrayOf(
        Typeface.createFromAsset(context.assets, "terminal/fonts/NotoSansSymbols-subset.ttf"),
        Typeface.createFromAsset(context.assets, "terminal/fonts/NotoSansSymbols2-subset.ttf"),
        Typeface.createFromAsset(context.assets, "terminal/fonts/NotoEmoji-subset.ttf"),
    )
    private val normalFont = if (Build.VERSION.SDK_INT >= 29) compositeFont(false) else
        Typeface.createFromAsset(context.assets, "terminal/fonts/JetBrainsMonoNerdFontMono-Regular.ttf")
    private val boldFont = if (Build.VERSION.SDK_INT >= 29) compositeFont(true) else
        Typeface.createFromAsset(context.assets, "terminal/fonts/JetBrainsMonoNerdFontMono-Bold.ttf")
    private val fallbackCache = android.util.LruCache<String, Typeface>(2048)
    private val textPaint = Paint(Paint.ANTI_ALIAS_FLAG or Paint.SUBPIXEL_TEXT_FLAG)
    private val fillPaint = Paint()
    private val decorationPaint = Paint(Paint.ANTI_ALIAS_FLAG)
    private val chromePaint = Paint(Paint.ANTI_ALIAS_FLAG)
    private val shape = Path()
    private val clip = Rect()
    private val historyButton = RectF()
    private val liveButton = RectF()
    private val reloadButton = RectF()
    private val scroller = OverScroller(context)
    private val touchSlop = ViewConfiguration.get(context).scaledTouchSlop
    private val minimumVelocity = ViewConfiguration.get(context).scaledMinimumFlingVelocity
    private val maximumVelocity = ViewConfiguration.get(context).scaledMaximumFlingVelocity
    private var cellWidth = 1f
    private var cellHeight = 1f
    private var baseline = 1f
    private var inset = 0f
    private var chromeHeight = 0f
    private var phoneColumns = 1
    private var inputEnabled = false
    private var disposed = false
    private var agentKind: String? = null
    private var frame: TerminalFrame? = null
    private var grid = mutableListOf<NativeTerminalRow>()
    private var liveLayout = emptyList<VisualRow>()
    private var historyLayout = emptyList<VisualRow>()
    private var liveSource = emptyList<NativeTerminalRow>()
    private val historySource = ArrayList<NativeTerminalRow>()
    private val historySizes = ArrayList<Int>()
    private val rowCache = IdentityHashMap<NativeTerminalRow, MutableMap<LayoutKey, RowGeometry>>()
    private var scrollYpx = 0f
    private var panX = 0f
    private var following = true
    private var historyVisible = false
    private var historyOpening = false
    private var historyLoading = false
    private var historyLoaded = false
    private var historyMore = true
    private var historyCursor: String? = null
    private var historyRevision: String? = null
    private var historyError: String? = null
    private var historyTruncated = false
    private var newerBoundary = false
    private var historyBytes = 0
    private var pendingHistoryScroll = 0f
    private var savedLiveScroll = 0f
    private var savedLivePan = 0f
    private var cursorRow = -1
    private var cursorColumn = -1
    private var cursorOn = true
    private var wheelDebt = 0f
    private var wheelPosted = false
    private var pointerX = 0f
    private var pointerY = 0f
    private var downX = 0f
    private var downY = 0f
    private var lastX = 0f
    private var lastY = 0f
    private var dragging = false
    private var multiplePointers = false
    private var selecting = false
    private var movingSelectionStart = false
    private var selectionStart: SourcePoint? = null
    private var selectionEnd: SourcePoint? = null
    private var actionMode: ActionMode? = null
    private var velocity: VelocityTracker? = null
    private var lastFlingY = 0
    private var connection: TerminalInputConnection? = null
    private var deadAccent = 0
    private var accessibilitySnapshot = ""
    private var accessibilityRevision: String? = null

    private data class SourcePoint(val row: NativeTerminalRow, val column: Int)
    private data class PositionedCell(val cell: NativeTerminalCell, val column: Int, val source: NativeTerminalRow)
    private class VisualRow {
        val cells = ArrayList<PositionedCell>()
        val ranges = ArrayList<SourceRange>()
    }
    private data class SourceRange(val source: NativeTerminalRow, val first: Int, val end: Int, val column: Int)
    private data class LayoutKey(val columns: Int, val start: Int, val end: Int)
    private data class RowGeometry(val visual: List<VisualRow>, val lastColumn: Int)
    private data class VisualPoint(val row: Int, val column: Int) : Comparable<VisualPoint> {
        override fun compareTo(other: VisualPoint): Int = if (row != other.row) row.compareTo(other.row) else column.compareTo(other.column)
    }
    private data class Anchor(val source: NativeTerminalRow?, val column: Int, val offset: Float, val fallback: Float)

    init {
        isFocusable = true
        isFocusableInTouchMode = true
        importantForAccessibility = IMPORTANT_FOR_ACCESSIBILITY_YES
        setWillNotDraw(false)
        updateMetrics()
        contentDescription = "终端。点击输入，长按选择文字，拖动滚动；顶部可打开历史或返回实时。"
    }

    fun reset() {
        stopMotion()
        finishSelection()
        connection?.resetBuffer()
        connection = null
        frame = null
        grid = mutableListOf()
        liveSource = emptyList()
        liveLayout = emptyList()
        rowCache.clear()
        clearHistory()
        scrollYpx = 0f
        panX = 0f
        following = true
        cursorRow = -1
        cursorColumn = -1
        inputEnabled = false
        agentKind = null
        deadAccent = 0
        accessibilitySnapshot = ""
        accessibilityRevision = null
        removeCallbacks(blink)
        hideKeyboard()
        invalidate()
    }

    fun setAppearance(dark: Boolean) {
        if (darkAppearance == dark) return
        darkAppearance = dark
        colors = NativeTerminalColors(dark)
        invalidate()
    }

    fun setInputEnabled(enabled: Boolean) {
        if (inputEnabled == enabled) return
        inputEnabled = enabled
        if (!enabled) {
            stopMotion()
            connection?.resetBuffer()
            connection = null
            hideKeyboard()
        }
        inputMethodManager.restartInput(this)
        invalidate()
    }

    fun setAgentKind(kind: String?) {
        if (agentKind == kind) return
        agentKind = kind
        invalidate(0, 0, width, chromeHeight.toInt())
    }

    fun sendShortcut(text: String) {
        if (!inputEnabled) return
        connection?.finishComposingText()
        connection?.resetBuffer()
        emit(text)
    }

    /** All grid state is swapped synchronously; the caller can acknowledge immediately. */
    fun applyScreen(frame: TerminalFrame, decoded: List<Pair<Int, NativeTerminalRow>>) {
        if (disposed) return
        val old = this.frame
        val anchor = captureAnchor()
        val full = frame.baseRevision == null || grid.size != frame.rows || old?.cols != frame.cols
        if (full) grid.forEach { rowCache.remove(it) }
        val next = if (full) MutableList(frame.rows) { NativeTerminalRow(frame.cols, emptyList()) } else grid
        for ((index, row) in decoded) {
            require(index in next.indices) { "Canonical row outside screen" }
            rowCache.remove(next[index])
            next[index] = row
        }
        grid = next
        this.frame = frame
        if (old?.alternate != frame.alternate) {
            stopMotion()
            finishSelection()
            if (!historyVisible) { panX = 0f; scrollYpx = 0f; following = true }
        }
        rebuildLive()
        if (!historyVisible) {
            if (following) scrollYpx = maxScroll(liveLayout)
            else restoreAnchor(anchor)
            clampScroll()
        }
        if (historyLoaded && historySource.isEmpty() && old?.revision != frame.revision && !historyLoading) {
            historyMore = true
            historyLoaded = false
        }
        cursorOn = true
        removeCallbacks(blink)
        if (isAttachedToWindow && frame.flagCursor("blink") && frame.flagCursor("visible")) postDelayed(blink, 550)
        // Accessibility text is assembled on demand, not for every repaint.
        accessibilityRevision = null
        postInvalidateOnAnimation()
    }

    fun applyHistory(page: HistoryPage, decoded: List<NativeTerminalRow>, sourceByteSizes: List<Int>) {
        if (disposed || !historyLoading) return
        val anchor = captureAnchor()
        val wasEmpty = historySource.isEmpty()
        val opening = historyOpening
        require(sourceByteSizes.size == decoded.size) { "History byte sizes do not match decoded rows" }
        historySource.addAll(0, decoded)
        historySizes.addAll(0, sourceByteSizes)
        historyBytes += sourceByteSizes.sum()
        while (historySource.size > 2000 || historyBytes > 2 * 1024 * 1024) {
            historyBytes -= historySizes.removeAt(historySizes.lastIndex)
            rowCache.remove(historySource.removeAt(historySource.lastIndex))
            newerBoundary = true
        }
        historyCursor = page.cursor
        historyMore = page.hasMore
        historyTruncated = historyTruncated || page.truncated
        historyLoaded = true
        historyLoading = false
        historyOpening = false
        historyError = null
        historyLayout = buildLayout(historySource, phoneColumns, null)
        if (opening && historySource.isNotEmpty()) {
            savedLiveScroll = scrollYpx
            savedLivePan = panX
            historyVisible = true
            panX = 0f
        }
        if (historyVisible) {
            if (wasEmpty) scrollYpx = max(0f, maxScroll(historyLayout) + pendingHistoryScroll)
            else restoreAnchor(anchor)
            clampScroll()
        }
        pendingHistoryScroll = 0f
        accessibilityRevision = null
        if (historySource.isEmpty() && !historyMore) returnLive()
        invalidate()
    }

    fun historyFailed(message: String) {
        if (!historyLoading) return
        historyLoading = false
        accessibilityRevision = null
        historyError = message
        historyMore = false
        historyCursor = null
        if (historyOpening) {
            historyOpening = false
            savedLiveScroll = scrollYpx
            savedLivePan = panX
            historyVisible = true
            scrollYpx = maxScroll(historyLayout)
        }
        invalidate()
        announceForAccessibility("历史加载失败：$message。可重新加载或返回实时。")
    }

    fun dispose() {
        if (disposed) return
        reset()
        disposed = true
        onInput = {}
        onHistory = {}
        connection = null
    }

    private val inputMethodManager get() = context.getSystemService(Context.INPUT_METHOD_SERVICE) as InputMethodManager
    private val activeLayout get() = if (historyVisible) historyLayout else liveLayout
    private val activeSource get() = if (historyVisible) historySource else liveSource
    private val viewportHeight get() = max(0f, height - chromeHeight)
    private val contentTop get() = chromeHeight + if (!historyVisible && frame?.alternate != true)
        max(0f, viewportHeight - liveLayout.size * cellHeight) else 0f
    private val showReturnLive get() = agentKind != "codex" && (historyVisible || historyOpening || !following)

    @android.annotation.TargetApi(29)
    private fun compositeFont(bold: Boolean): Typeface {
        val regular = Font.Builder(context.assets, "terminal/fonts/JetBrainsMonoNerdFontMono-Regular.ttf").build()
        val heavy = Font.Builder(context.assets, "terminal/fonts/JetBrainsMonoNerdFontMono-Bold.ttf").build()
        val primary = FontFamily.Builder(regular).addFont(heavy).build()
        val builder = Typeface.CustomFallbackBuilder(primary)
        for (name in arrayOf("NotoSansSymbols-subset.ttf", "NotoSansSymbols2-subset.ttf", "NotoEmoji-subset.ttf")) {
            builder.addCustomFallback(FontFamily.Builder(Font.Builder(context.assets, "terminal/fonts/$name").build()).build())
        }
        return builder.setSystemFallback("monospace").setStyle(FontStyle(if (bold) 700 else 400, FontStyle.FONT_SLANT_UPRIGHT)).build()
    }

    private fun fontFor(text: String, bold: Boolean): Typeface {
        val primary = if (bold) boldFont else normalFont
        if (Build.VERSION.SDK_INT >= 29) return primary
        // Old Android cannot build a custom fallback chain. Cache a glyph-level choice instead.
        val key = (if (bold) "b" else "r") + text
        fallbackCache.get(key)?.let { return it }
        textPaint.typeface = primary
        var selected = primary
        if (!textPaint.hasGlyph(text)) {
            for (font in fallbackFonts) {
                textPaint.typeface = font
                if (textPaint.hasGlyph(text)) {
                    selected = if (bold) Typeface.create(font, Typeface.BOLD) else font
                    break
                }
            }
        }
        fallbackCache.put(key, selected)
        return selected
    }

    private fun updateMetrics() {
        val density = resources.displayMetrics.density
        @Suppress("DEPRECATION")
        val scaledDensity = resources.displayMetrics.scaledDensity
        textPaint.textSize = 14f * scaledDensity
        textPaint.typeface = normalFont
        textPaint.textScaleX = 1f
        textPaint.textSkewX = 0f
        cellWidth = max(1f, textPaint.measureText("M"))
        val metrics = textPaint.fontMetrics
        cellHeight = ceil(max(textPaint.textSize * 1.3f, metrics.descent - metrics.ascent)).coerceAtLeast(1f)
        baseline = (cellHeight - (metrics.descent - metrics.ascent)) / 2f - metrics.ascent
        inset = 8f * density
        chromeHeight = 36f * density
        chromePaint.textSize = 12f * scaledDensity
        chromePaint.typeface = Typeface.create(Typeface.DEFAULT, Typeface.NORMAL)
        phoneColumns = max(1, floor((width - inset * 2) / cellWidth).toInt())
        decorationPaint.strokeWidth = max(1f, density)
    }

    override fun onConfigurationChanged(newConfig: Configuration) {
        super.onConfigurationChanged(newConfig)
        val anchor = captureAnchor()
        updateMetrics()
        rowCache.clear()
        rebuildLive()
        historyLayout = buildLayout(historySource, phoneColumns, null)
        if (following && !historyVisible) scrollYpx = maxScroll(liveLayout) else restoreAnchor(anchor)
        clampScroll()
        invalidate()
    }

    override fun onSizeChanged(w: Int, h: Int, oldw: Int, oldh: Int) {
        val anchor = captureAnchor()
        updateMetrics()
        rowCache.clear()
        rebuildLive()
        historyLayout = buildLayout(historySource, phoneColumns, null)
        if (following && !historyVisible) scrollYpx = maxScroll(liveLayout) else restoreAnchor(anchor)
        clampScroll()
    }

    private fun rebuildLive() {
        val state = frame ?: return
        var last = grid.lastIndex
        val remoteCursorRow = state.cursor.number("row")
        if (!state.alternate) {
            while (last > remoteCursorRow && grid[last].cells.isEmpty()) last--
        }
        liveSource = if (last < 0) emptyList() else grid.subList(0, last + 1)
        liveLayout = buildLayout(liveSource, if (state.alternate) state.cols else phoneColumns, state)
        cursorRow = -1
        cursorColumn = -1
        val source = grid.getOrNull(remoteCursorRow)
        if (source != null) {
            locate(SourcePoint(source, state.cursor.number("col")), liveLayout)?.let {
                cursorRow = it.row
                cursorColumn = it.column
            }
        }
    }

    /** Cached geometry is keyed by canonical row identity and its incoming reflow column. */
    private fun geometry(source: NativeTerminalRow, columns: Int, start: Int, end: Int): RowGeometry {
        val key = LayoutKey(columns, start, end)
        val cache = rowCache.getOrPut(source) { HashMap() }
        cache[key]?.let { return it }
        var outputRow = 0
        var outputColumn = start
        var sourceColumn = 0
        val visual = ArrayList<VisualRow>()
        visual.add(VisualRow())
        var rangeStart = 0
        var rangeColumn = start
        fun newline() {
            visual[outputRow].ranges.add(SourceRange(source, rangeStart, sourceColumn, rangeColumn))
            visual.add(VisualRow())
            outputRow++
            outputColumn = 0
            rangeStart = sourceColumn
            rangeColumn = 0
        }
        fun gap(until: Int) {
            while (sourceColumn < until) {
                if (outputColumn >= columns) newline()
                val amount = min(until - sourceColumn, columns - outputColumn)
                sourceColumn += amount
                outputColumn += amount
            }
        }
        for (cell in source.cells) {
            if (cell.column >= end) break
            gap(min(end, cell.column))
            if (outputColumn >= columns || outputColumn > 0 && outputColumn + cell.width > columns) newline()
            visual[outputRow].cells.add(PositionedCell(cell, outputColumn, source))
            outputColumn += cell.width
            sourceColumn = cell.column + cell.width
        }
        gap(end)
        visual[outputRow].ranges.add(SourceRange(source, rangeStart, end, rangeColumn))
        val result = RowGeometry(visual, outputColumn)
        // Cursor extension creates at most a handful of variants for an otherwise unchanged row.
        if (cache.size >= 8) cache.clear()
        cache[key] = result
        return result
    }

    private fun buildLayout(source: List<NativeTerminalRow>, columns: Int, state: TerminalFrame?): List<VisualRow> {
        if (source.isEmpty()) return emptyList()
        val output = ArrayList<VisualRow>()
        var joining = false
        var carriedColumn = 0
        val alternate = state?.alternate == true
        for ((sourceIndex, row) in source.withIndex()) {
            val contentEnd = row.cells.lastOrNull()?.let { it.column + it.width } ?: 0
            val cursorEnd = if (state != null && state.cursor.number("row") == sourceIndex) state.cursor.number("col") + 1 else 0
            val end = when {
                alternate -> row.cols
                row.wrapped -> row.cols - if (row.wrapPadding) 1 else 0
                else -> max(contentEnd, cursorEnd)
            }.coerceIn(0, row.cols)
            val layout = geometry(row, columns.coerceAtLeast(1), carriedColumn, end)
            if (!joining) output.addAll(layout.visual)
            else {
                val last = output.last()
                val incoming = layout.visual.first()
                val merged = VisualRow()
                merged.cells.addAll(last.cells)
                merged.cells.addAll(incoming.cells)
                merged.ranges.addAll(last.ranges)
                merged.ranges.addAll(incoming.ranges)
                output[output.lastIndex] = merged
                for (index in 1 until layout.visual.size) output.add(layout.visual[index])
            }
            joining = !alternate && row.wrapped
            carriedColumn = if (joining) layout.lastColumn else 0
        }
        return output
    }


    private fun maxScroll(layout: List<VisualRow> = activeLayout) = max(0f, layout.size * cellHeight - viewportHeight)
    private fun maxPan() = if (!historyVisible && frame?.alternate == true) max(0f, (frame?.cols ?: 0) * cellWidth + inset * 2 - width) else 0f
    private fun clampScroll() {
        scrollYpx = scrollYpx.coerceIn(0f, maxScroll())
        panX = panX.coerceIn(0f, maxPan())
    }

    private fun captureAnchor(): Anchor {
        val index = floor(scrollYpx / cellHeight).toInt().coerceAtLeast(0)
        val range = activeLayout.getOrNull(index)?.ranges?.firstOrNull()
        return Anchor(range?.source, range?.first ?: 0, scrollYpx - index * cellHeight, scrollYpx)
    }

    private fun restoreAnchor(anchor: Anchor) {
        val row = anchor.source
        val position = if (row == null) null else locate(SourcePoint(row, anchor.column), activeLayout)
        scrollYpx = if (position != null) position.row * cellHeight + anchor.offset else anchor.fallback
    }

    private fun locate(point: SourcePoint, layout: List<VisualRow> = activeLayout): VisualPoint? {
        var endPoint: VisualPoint? = null
        for ((rowIndex, row) in layout.withIndex()) {
            for (range in row.ranges) if (range.source === point.row) {
                if (point.column >= range.first && point.column < range.end) return VisualPoint(rowIndex, range.column + point.column - range.first)
                if (point.column == range.end) endPoint = VisualPoint(rowIndex, range.column + range.end - range.first)
            }
        }
        return endPoint
    }

    override fun onDraw(canvas: Canvas) {
        // drawColor fills the parent's clip, ignoring this AndroidView's translated bounds.
        fillPaint.color = colors.background
        canvas.drawRect(0f, 0f, width.toFloat(), height.toFloat(), fillPaint)
        drawChrome(canvas)
        if (activeLayout.isEmpty() || viewportHeight <= 0) return
        canvas.save()
        canvas.clipRect(0f, chromeHeight, width.toFloat(), height.toFloat())
        canvas.getClipBounds(clip)
        val first = max(0, floor(scrollYpx / cellHeight).toInt())
        val last = min(activeLayout.lastIndex, ceil((scrollYpx + viewportHeight) / cellHeight).toInt())
        val start = selectionStart?.let { locate(it) }
        val end = selectionEnd?.let { locate(it) }
        val low = if (start != null && end != null) minOf(start, end) else null
        val high = if (start != null && end != null) maxOf(start, end) else null
        for (index in first..last) {
            val row = activeLayout[index]
            val top = contentTop + index * cellHeight - scrollYpx
            // Paint the entire row's backgrounds before glyphs: adjacent Powerline cells share edges.
            for (placed in row.cells) {
                val cell = placed.cell
                val x = inset + placed.column * cellWidth - panX
                if (x + cell.width * cellWidth < 0 || x > width) continue
                fillPaint.color = background(cell.style)
                canvas.drawRect(x, top, x + cell.width * cellWidth, top + cellHeight, fillPaint)
            }
            if (low != null && high != null && index in low.row..high.row) {
                val firstCol = if (index == low.row) low.column else 0
                val lastCol = if (index == high.row) high.column else if (!historyVisible && frame?.alternate == true) frame?.cols ?: phoneColumns else phoneColumns
                fillPaint.color = 0x665b9dff
                canvas.drawRect(inset + firstCol * cellWidth - panX, top, inset + lastCol * cellWidth - panX, top + cellHeight, fillPaint)
            }
            for (placed in row.cells) drawCell(canvas, placed, top)
            if (!historyVisible && cursorRow == index && frame?.flagCursor("visible") == true && (!frame!!.flagCursor("blink") || cursorOn || !hasFocus())) drawCursor(canvas, row, top)
        }
        drawSelectionHandle(canvas, start)
        drawSelectionHandle(canvas, end)
        canvas.restore()
    }

    private fun foreground(style: NativeTerminalStyle) = colors.resolve(if (style.inverse) style.background else style.foreground)
    private fun background(style: NativeTerminalStyle) = colors.resolve(if (style.inverse) style.foreground else style.background)

    private fun drawCell(canvas: Canvas, placed: PositionedCell, top: Float) {
        val cell = placed.cell
        val style = cell.style
        val x = inset + placed.column * cellWidth - panX
        val cellPixels = cell.width * cellWidth
        if (style.hidden || x + cellPixels < 0 || x > width) return
        val fg = foreground(style)
        textPaint.color = fg
        textPaint.alpha = if (style.dim) 128 else 255
        textPaint.typeface = fontFor(cell.text, style.bold)
        textPaint.textSkewX = if (style.italic) -.2f else 0f
        canvas.save()
        canvas.clipRect(x, top, x + cellPixels, top + cellHeight)
        if (!drawSolidGlyph(canvas, cell.text, x, top, cellPixels, textPaint.color, textPaint.alpha)) {
            canvas.drawText(cell.text, x, top + baseline, textPaint)
        }
        decorationPaint.color = style.underlineColor?.let(colors::resolve) ?: fg
        decorationPaint.alpha = if (style.dim) 128 else 255
        decorationPaint.style = Paint.Style.STROKE
        if (style.underline != 0) {
            val y = min(top + cellHeight - decorationPaint.strokeWidth, top + baseline + decorationPaint.strokeWidth * 1.5f)
            when (style.underline) {
                2 -> {
                    canvas.drawLine(x, y - decorationPaint.strokeWidth * 2, x + cellPixels, y - decorationPaint.strokeWidth * 2, decorationPaint)
                    canvas.drawLine(x, y, x + cellPixels, y, decorationPaint)
                }
                3 -> {
                    shape.reset()
                    shape.moveTo(x, y)
                    val step = max(2f, decorationPaint.strokeWidth * 2)
                    var offset = 0f
                    var up = true
                    while (offset < cellPixels) {
                        offset = min(cellPixels, offset + step)
                        shape.lineTo(x + offset, y + if (up) -decorationPaint.strokeWidth else decorationPaint.strokeWidth)
                        up = !up
                    }
                    canvas.drawPath(shape, decorationPaint)
                }
                4, 5 -> {
                    val step = decorationPaint.strokeWidth * if (style.underline == 4) 2 else 4
                    var offset = 0f
                    while (offset < cellPixels) {
                        canvas.drawLine(x + offset, y, x + min(cellPixels, offset + step / 2), y, decorationPaint)
                        offset += step
                    }
                }
                else -> canvas.drawLine(x, y, x + cellPixels, y, decorationPaint)
            }
        }
        if (style.strike) {
            decorationPaint.color = fg
            canvas.drawLine(x, top + baseline - textPaint.textSize * .3f, x + cellPixels, top + baseline - textPaint.textSize * .3f, decorationPaint)
        }
        canvas.restore()
    }

    /** These solid separators/blocks must meet cell edges instead of a font's side bearings. */
    private fun drawSolidGlyph(canvas: Canvas, text: String, x: Float, y: Float, w: Float, color: Int, alpha: Int): Boolean {
        if (text.length != 1) return false
        fillPaint.color = color
        fillPaint.alpha = alpha
        fillPaint.isAntiAlias = false
        val h = cellHeight
        when (text[0]) {
            '\ue0b0', '\ue0b2' -> {
                val right = text[0] == '\ue0b0'
                shape.reset()
                shape.moveTo(if (right) x else x + w, y)
                shape.lineTo(if (right) x + w else x, y + h / 2)
                shape.lineTo(if (right) x else x + w, y + h)
                shape.close()
                canvas.drawPath(shape, fillPaint)
            }
            '\ue0b4', '\ue0b6' -> {
                shape.reset()
                val right = text[0] == '\ue0b4'
                shape.moveTo(if (right) x else x + w, y)
                shape.cubicTo(if (right) x + w * 1.34f else x - w * .34f, y, if (right) x + w * 1.34f else x - w * .34f, y + h, if (right) x else x + w, y + h)
                shape.close()
                canvas.drawPath(shape, fillPaint)
            }
            '\u2580' -> canvas.drawRect(x, y, x + w, y + h / 2, fillPaint)
            '\u2584' -> canvas.drawRect(x, y + h / 2, x + w, y + h, fillPaint)
            '\u2588' -> canvas.drawRect(x, y, x + w, y + h, fillPaint)
            '\u258c' -> canvas.drawRect(x, y, x + w / 2, y + h, fillPaint)
            '\u2590' -> canvas.drawRect(x + w / 2, y, x + w, y + h, fillPaint)
            else -> { fillPaint.alpha = 255; return false }
        }
        fillPaint.alpha = 255
        return true
    }

    private fun drawCursor(canvas: Canvas, row: VisualRow, top: Float) {
        val x = inset + cursorColumn * cellWidth - panX
        val style = frame?.cursor?.get("shape")?.jsonPrimitive?.content ?: "block"
        fillPaint.color = colors.foreground
        fillPaint.alpha = if (inputEnabled) 220 else 110
        val stroke = max(1f, resources.displayMetrics.density * 1.5f)
        when (style) {
            "bar" -> canvas.drawRect(x, top, x + stroke, top + cellHeight, fillPaint)
            "underline" -> canvas.drawRect(x, top + cellHeight - stroke, x + cellWidth, top + cellHeight, fillPaint)
            else -> {
                val cell = row.cells.firstOrNull { cursorColumn >= it.column && cursorColumn < it.column + it.cell.width }
                val cursorWidth = (cell?.cell?.width ?: 1) * cellWidth
                val cursorX = if (cell == null) x else inset + cell.column * cellWidth - panX
                canvas.drawRect(cursorX, top, cursorX + cursorWidth, top + cellHeight, fillPaint)
                if (cell != null && !cell.cell.style.hidden) {
                    canvas.save()
                    canvas.clipRect(cursorX, top, cursorX + cursorWidth, top + cellHeight)
                    textPaint.color = colors.background
                    textPaint.alpha = 255
                    textPaint.typeface = fontFor(cell.cell.text, cell.cell.style.bold)
                    textPaint.textSkewX = if (cell.cell.style.italic) -.2f else 0f
                    canvas.drawText(cell.cell.text, cursorX, top + baseline, textPaint)
                    canvas.restore()
                }
            }
        }
        fillPaint.alpha = 255
    }

    private fun drawChrome(canvas: Canvas) {
        val density = resources.displayMetrics.density
        fillPaint.color = colors.background
        canvas.drawRect(0f, 0f, width.toFloat(), chromeHeight, fillPaint)
        decorationPaint.color = colors.foreground
        decorationPaint.alpha = 35
        canvas.drawLine(0f, chromeHeight - 1, width.toFloat(), chromeHeight - 1, decorationPaint)
        decorationPaint.alpha = 255
        val buttonWidth = max(48f * density, chromePaint.measureText("重新加载") + inset * 2)
        historyButton.set(0f, 0f, buttonWidth, chromeHeight)
        liveButton.set(max(0f, width - buttonWidth), 0f, width.toFloat(), chromeHeight)
        reloadButton.set(max(buttonWidth, width - buttonWidth * 2), 0f, max(buttonWidth, width - buttonWidth), chromeHeight)
        chromePaint.color = colors.foreground
        chromePaint.alpha = if (inputEnabled) 255 else 110
        val y = chromeHeight / 2 - (chromePaint.ascent() + chromePaint.descent()) / 2
        canvas.drawText(if (historyVisible || historyOpening) "历史" else "主屏历史", inset, y, chromePaint)
        if (showReturnLive) {
            canvas.drawText("返回实时", liveButton.left + inset / 2, y, chromePaint)
        }
        val canReload = (historyVisible || historyOpening) && !historyLoading
        if (canReload) canvas.drawText("重新加载", reloadButton.left + inset / 2, y, chromePaint)
        val status = when {
            historyLoading -> "正在加载…"
            historyError != null -> "加载失败"
            newerBoundary -> "较新内容可重载"
            historyVisible && !historyMore && historyTruncated -> "已到保留边界"
            historyVisible && !historyMore -> "历史起点"
            historyVisible -> "上滑加载更多"
            !inputEnabled -> "连接中…"
            frame?.alternate == true -> if (agentKind == "codex") "Codex" else "应用屏幕"
            else -> ""
        }
        val right = if (canReload) reloadButton.left else if (showReturnLive) liveButton.left else width.toFloat()
        if (right > historyButton.right + inset) {
            canvas.save()
            canvas.clipRect(historyButton.right, 0f, right, chromeHeight)
            chromePaint.alpha = 170
            canvas.drawText(status, historyButton.right + inset / 2, y, chromePaint)
            canvas.restore()
        }
        chromePaint.alpha = 255
    }

    private val blink = object : Runnable {
        override fun run() {
            if (disposed || !isAttachedToWindow || frame?.flagCursor("blink") != true) return
            cursorOn = !cursorOn
            if (!historyVisible && cursorRow >= 0) {
                val top = (contentTop + cursorRow * cellHeight - scrollYpx).toInt()
                invalidate(0, max(chromeHeight.toInt(), top), width, min(height, ceil(top + cellHeight).toInt()))
            }
            postDelayed(this, 550)
        }
    }

    private fun clearHistory() {
        accessibilityRevision = null
        historySource.forEach { rowCache.remove(it) }
        historySource.clear()
        historySizes.clear()
        historyLayout = emptyList()
        historyBytes = 0
        historyVisible = false
        historyOpening = false
        historyLoading = false
        historyLoaded = false
        historyMore = true
        historyCursor = null
        historyRevision = null
        historyError = null
        historyTruncated = false
        newerBoundary = false
        pendingHistoryScroll = 0f
    }

    private fun openHistory(reload: Boolean = false) {
        if (!inputEnabled || historyLoading) return
        stopMotion()
        finishSelection()
        if (reload || historyRevision != frame?.revision || newerBoundary || historyError != null) {
            if (historyVisible) {
                scrollYpx = savedLiveScroll
                panX = savedLivePan
            }
            clearHistory()
        }
        following = false
        if (historyLoaded && historySource.isNotEmpty()) {
            savedLiveScroll = scrollYpx
            savedLivePan = panX
            historyVisible = true
            panX = 0f
            scrollYpx = maxScroll(historyLayout)
        } else {
            historyOpening = true
            requestHistory()
        }
        invalidate()
    }

    private fun requestHistory() {
        if (!inputEnabled || historyLoading || !historyMore) return
        if (!historyLoaded) historyRevision = frame?.revision
        historyLoading = true
        accessibilityRevision = null
        historyError = null
        invalidate(0, 0, width, chromeHeight.toInt())
        onHistory(historyCursor)
    }

    private fun returnLive() {
        stopMotion()
        finishSelection()
        historyVisible = false
        historyOpening = false
        pendingHistoryScroll = 0f
        panX = savedLivePan.coerceIn(0f, maxPan())
        scrollYpx = maxScroll(liveLayout)
        following = true
        accessibilityRevision = null
        invalidate()
    }

    private fun applicationScrolls() = !historyVisible && frame?.alternate == true && (mode("mouse") || mode("alternate_scroll")) && inputEnabled

    private fun scrollAmount(amount: Float, forceLocal: Boolean = false) {
        if (historyOpening) {
            pendingHistoryScroll = min(0f, pendingHistoryScroll + amount)
            if (amount > 0 && pendingHistoryScroll == 0f) returnLive()
            return
        }
        if (!forceLocal && applicationScrolls()) {
            if (wheelDebt * amount < 0) wheelDebt = 0f
            wheelDebt += amount / cellHeight
            if (!wheelPosted && abs(wheelDebt) >= 1) {
                wheelPosted = true
                postDelayed(flushWheel, 32)
            }
            return
        }
        val before = scrollYpx
        scrollYpx = (scrollYpx + amount).coerceIn(0f, maxScroll())
        if (historyVisible) {
            if (amount > 0 && scrollYpx >= maxScroll() && !newerBoundary && !historyLoading && historyError == null) {
                returnLive()
                return
            }
            if (amount < 0 && scrollYpx < 160f * resources.displayMetrics.density) requestHistory()
        } else {
            following = scrollYpx >= maxScroll() - 1
            // Full-screen apps without wheel modes pan their canonical grid, never shell history.
            if (amount < 0 && before + amount < 0 && frame?.alternate != true && inputEnabled) {
                openHistory()
                pendingHistoryScroll = min(0f, before + amount)
            }
        }
        invalidate()
    }

    private val flushWheel = object : Runnable {
        override fun run() {
            wheelPosted = false
            if (!applicationScrolls()) { wheelDebt = 0f; return }
            val lines = wheelDebt.toInt().coerceIn(-20, 20)
            if (lines == 0) return
            wheelDebt -= lines
            val sequence = if (mode("mouse")) {
                val cols = frame?.cols ?: 1
                val rows = frame?.rows ?: 1
                val col = (floor((pointerX + panX - inset) / cellWidth).toInt() + 1).coerceIn(1, cols)
                val row = (floor((pointerY + scrollYpx - chromeHeight) / cellHeight).toInt() + 1).coerceIn(1, rows)
                mouseSequence(if (lines < 0) 64 else 65, col, row, false)
            } else arrow(if (lines < 0) 'A' else 'B')
            emit(sequence.repeat(abs(lines)), returnToBottom = false, resetMotion = false)
            if (abs(wheelDebt) >= 1) { wheelPosted = true; postDelayed(this, 32) }
        }
    }

    private fun stopMotion() {
        scroller.abortAnimation()
        removeCallbacks(flushWheel)
        removeCallbacks(longPress)
        removeCallbacks(selectionAutoScroll)
        wheelPosted = false
        wheelDebt = 0f
    }

    override fun onTouchEvent(event: MotionEvent): Boolean {
        if (disposed) return false
        pointerX = event.x
        pointerY = event.y
        when (event.actionMasked) {
            MotionEvent.ACTION_DOWN -> {
                stopMotion()
                parent?.requestDisallowInterceptTouchEvent(true)
                velocity?.recycle()
                velocity = VelocityTracker.obtain().also { it.addMovement(event) }
                downX = event.x; lastX = event.x
                downY = event.y; lastY = event.y
                dragging = false
                multiplePointers = false
                selecting = false
                if (selectionStart != null && event.y >= chromeHeight) {
                    val a = selectionStart?.let { locate(it) }
                    val b = selectionEnd?.let { locate(it) }
                    val da = distanceTo(a, event.x, event.y)
                    val db = distanceTo(b, event.x, event.y)
                    val radius = 32 * resources.displayMetrics.density
                    if (min(da, db) < radius * radius) {
                        selecting = true
                        movingSelectionStart = da <= db
                    }
                }
                if (!selecting && event.y >= chromeHeight) postDelayed(longPress, ViewConfiguration.getLongPressTimeout().toLong())
                return true
            }
            MotionEvent.ACTION_POINTER_DOWN -> {
                multiplePointers = true
                removeCallbacks(longPress)
                selecting = false
                dragging = true
                lastX = event.x; lastY = event.y
                return true
            }
            MotionEvent.ACTION_MOVE -> {
                velocity?.addMovement(event)
                if (selecting) {
                    updateSelection(event.x, event.y)
                    return true
                }
                if (!dragging && (abs(event.x - downX) > touchSlop || abs(event.y - downY) > touchSlop)) {
                    dragging = true
                    removeCallbacks(longPress)
                }
                if (dragging) {
                    val dx = lastX - event.x
                    val dy = lastY - event.y
                    if (maxPan() > 0 && (multiplePointers || abs(event.x - downX) > abs(event.y - downY))) {
                        panX = (panX + dx).coerceIn(0f, maxPan())
                        invalidate()
                    } else scrollAmount(dy, multiplePointers)
                }
                lastX = event.x; lastY = event.y
                return true
            }
            MotionEvent.ACTION_POINTER_UP -> {
                multiplePointers = true
                velocity?.clear()
                val remaining = if (event.actionIndex == 0 && event.pointerCount > 1) 1 else 0
                lastX = event.getX(remaining); lastY = event.getY(remaining)
                return true
            }
            MotionEvent.ACTION_UP -> {
                removeCallbacks(longPress)
                removeCallbacks(selectionAutoScroll)
                velocity?.addMovement(event)
                if (selecting) {
                    selecting = false
                    showSelectionActions()
                } else if (!dragging && !multiplePointers) {
                    if (event.y < chromeHeight) handleChromeTap(event.x, event.y)
                    else {
                        finishSelection()
                        if (!historyVisible && !historyOpening && inputEnabled) focusKeyboard()
                        performClick()
                    }
                } else if (!multiplePointers && dragging && abs(event.y - downY) > abs(event.x - downX)) {
                    velocity?.computeCurrentVelocity(1000, maximumVelocity.toFloat())
                    val speed = -(velocity?.yVelocity ?: 0f)
                    if (abs(speed) >= minimumVelocity) {
                        lastFlingY = 0
                        if (applicationScrolls()) scroller.startScroll(0, 0, 0, (speed * .12f).toInt(), 240)
                        else scroller.fling(0, 0, 0, speed.toInt(), 0, 0, -100000, 100000)
                        postInvalidateOnAnimation()
                    }
                }
                velocity?.recycle(); velocity = null
                parent?.requestDisallowInterceptTouchEvent(false)
                return true
            }
            MotionEvent.ACTION_CANCEL -> {
                stopMotion()
                selecting = false
                velocity?.recycle(); velocity = null
                parent?.requestDisallowInterceptTouchEvent(false)
                return true
            }
        }
        return true
    }

    override fun performClick(): Boolean { super.performClick(); return true }

    private fun handleChromeTap(x: Float, y: Float) {
        when {
            liveButton.contains(x, y) && showReturnLive -> returnLive()
            reloadButton.contains(x, y) && (historyVisible || historyOpening) && !historyLoading -> openHistory(reload = true)
            historyButton.contains(x, y) -> if (historyVisible && scrollYpx < 160f) requestHistory() else openHistory()
        }
    }

    override fun computeScroll() {
        if (!scroller.computeScrollOffset()) return
        val dy = scroller.currY - lastFlingY
        lastFlingY = scroller.currY
        if (dy != 0) scrollAmount(dy.toFloat())
        if (!scroller.isFinished) postInvalidateOnAnimation()
    }

    override fun onGenericMotionEvent(event: MotionEvent): Boolean {
        if (event.action == MotionEvent.ACTION_SCROLL) {
            pointerX = event.x; pointerY = event.y
            stopMotion()
            panX = (panX - event.getAxisValue(MotionEvent.AXIS_HSCROLL) * cellWidth * 3).coerceIn(0f, maxPan())
            scrollAmount(-event.getAxisValue(MotionEvent.AXIS_VSCROLL) * cellHeight * 3, event.metaState and KeyEvent.META_SHIFT_MASK != 0)
            return true
        }
        return super.onGenericMotionEvent(event)
    }

    private val longPress = Runnable {
        if (!dragging && !multiplePointers && downY >= chromeHeight) {
            val point = pointAt(downX, downY) ?: return@Runnable
            val cell = point.row.cells.firstOrNull { point.column >= it.column && point.column < it.column + it.width }
            val first = cell?.column ?: point.column
            val end = cell?.let { it.column + it.width } ?: first + 1
            selectionStart = SourcePoint(point.row, first)
            selectionEnd = SourcePoint(point.row, end)
            selecting = true
            movingSelectionStart = false
            performHapticFeedback(android.view.HapticFeedbackConstants.LONG_PRESS)
            invalidate()
            showSelectionActions()
        }
    }

    private fun pointAt(x: Float, y: Float): SourcePoint? {
        val row = activeLayout.getOrNull(floor((y - contentTop + scrollYpx) / cellHeight).toInt().coerceIn(0, max(0, activeLayout.lastIndex))) ?: return null
        val column = floor((x + panX - inset) / cellWidth).toInt().coerceAtLeast(0)
        for (placed in row.cells) if (column >= placed.column && column < placed.column + placed.cell.width) return SourcePoint(placed.source, placed.cell.column)
        val range = row.ranges.firstOrNull { column >= it.column && column < it.column + it.end - it.first } ?: row.ranges.lastOrNull() ?: return null
        return SourcePoint(range.source, (range.first + column - range.column).coerceIn(range.first, range.end))
    }

    private fun updateSelection(x: Float, y: Float) {
        val point = pointAt(x, y.coerceIn(chromeHeight, max(chromeHeight, height.toFloat() - 1))) ?: return
        if (movingSelectionStart) selectionStart = point else {
            val cell = point.row.cells.firstOrNull { point.column >= it.column && point.column < it.column + it.width }
            selectionEnd = SourcePoint(point.row, cell?.let { it.column + it.width } ?: point.column)
        }
        removeCallbacks(selectionAutoScroll)
        val edge = 28f * resources.displayMetrics.density
        if (y < chromeHeight + edge || y > height - edge) postDelayed(selectionAutoScroll, 32)
        actionMode?.invalidateContentRect()
        invalidate()
    }

    private val selectionAutoScroll = object : Runnable {
        override fun run() {
            if (!selecting) return
            val edge = 28f * resources.displayMetrics.density
            val delta = when {
                pointerY < chromeHeight + edge -> -cellHeight
                pointerY > height - edge -> cellHeight
                else -> return
            }
            // Selection scroll is always local, never mouse-wheel input to the application.
            scrollYpx = (scrollYpx + delta).coerceIn(0f, maxScroll())
            updateSelection(pointerX, pointerY)
        }
    }

    private fun distanceTo(point: VisualPoint?, x: Float, y: Float): Float {
        if (point == null) return Float.MAX_VALUE
        val dx = inset + point.column * cellWidth - panX - x
        val dy = contentTop + (point.row + 1) * cellHeight - scrollYpx - y
        return dx * dx + dy * dy
    }

    private fun drawSelectionHandle(canvas: Canvas, point: VisualPoint?) {
        if (point == null) return
        val x = inset + point.column * cellWidth - panX
        val y = contentTop + (point.row + 1) * cellHeight - scrollYpx
        fillPaint.color = 0xff5b9dff.toInt()
        canvas.drawCircle(x, y, 5f * resources.displayMetrics.density, fillPaint)
    }

    private fun finishSelection() {
        actionMode?.finish()
        actionMode = null
        selectionStart = null
        selectionEnd = null
        selecting = false
        removeCallbacks(selectionAutoScroll)
    }

    private fun showSelectionActions() {
        if (actionMode != null) { actionMode?.invalidate(); return }
        actionMode = startActionMode(object : ActionMode.Callback2() {
            override fun onCreateActionMode(mode: ActionMode, menu: Menu): Boolean {
                menu.add(0, android.R.id.copy, 0, "复制").setShowAsAction(MenuItem.SHOW_AS_ACTION_IF_ROOM)
                menu.add(0, android.R.id.paste, 1, "粘贴").setShowAsAction(MenuItem.SHOW_AS_ACTION_IF_ROOM)
                menu.add(0, android.R.id.selectAll, 2, "全选")
                return true
            }
            override fun onPrepareActionMode(mode: ActionMode, menu: Menu): Boolean {
                menu.findItem(android.R.id.paste)?.isEnabled = inputEnabled && clipboard.hasPrimaryClip()
                return true
            }
            override fun onActionItemClicked(mode: ActionMode, item: MenuItem): Boolean = when (item.itemId) {
                android.R.id.copy -> { copySelection(); mode.finish(); true }
                android.R.id.paste -> { pasteClipboard(); mode.finish(); true }
                android.R.id.selectAll -> { selectAll(); true }
                else -> false
            }
            override fun onDestroyActionMode(mode: ActionMode) {
                actionMode = null
                selectionStart = null
                selectionEnd = null
                selecting = false
                invalidate()
            }
            override fun onGetContentRect(mode: ActionMode, view: View, outRect: Rect) {
                val point = selectionEnd?.let { locate(it) }
                val y = if (point == null) chromeHeight.toInt() else (contentTop + point.row * cellHeight - scrollYpx).toInt()
                outRect.set(inset.toInt(), y.coerceIn(chromeHeight.toInt(), max(chromeHeight.toInt(), height - cellHeight.toInt())), width - inset.toInt(), (y + cellHeight).toInt().coerceIn(chromeHeight.toInt(), max(chromeHeight.toInt(), height)))
            }
        }, ActionMode.TYPE_FLOATING)
    }

    private fun selectedText(): String {
        val first = selectionStart ?: return ""
        val last = selectionEnd ?: return ""
        val startVisual = locate(first) ?: return ""
        val endVisual = locate(last) ?: return ""
        val low = if (startVisual <= endVisual) first else last
        val high = if (startVisual <= endVisual) last else first
        val lowIndex = activeSource.indexOfFirst { it === low.row }
        val highIndex = activeSource.indexOfFirst { it === high.row }
        if (lowIndex < 0 || highIndex < lowIndex) return ""
        return buildString {
            for (index in lowIndex..highIndex) {
                val row = activeSource[index]
                val from = if (index == lowIndex) low.column else 0
                val to = if (index == highIndex) high.column else if (row.wrapped) row.cols - if (row.wrapPadding) 1 else 0 else row.cells.lastOrNull()?.let { it.column + it.width } ?: 0
                var column = from
                for (cell in row.cells) {
                    if (cell.column + cell.width <= from) continue
                    if (cell.column >= to) break
                    while (column < cell.column) { append(' '); column++ }
                    if (cell.style.hidden) repeat(cell.width) { append(' ') } else append(cell.text)
                    column = cell.column + cell.width
                }
                // Unused grid padding is not copied, but meaningful spaces in a wrapped row are.
                if (row.wrapped) while (column < to) { append(' '); column++ }
                if (index != highIndex && !row.wrapped) append('\n')
            }
        }
    }

    private val clipboard get() = context.getSystemService(Context.CLIPBOARD_SERVICE) as ClipboardManager
    private fun copySelection() {
        val text = selectedText()
        if (text.isNotEmpty()) clipboard.setPrimaryClip(ClipData.newPlainText("终端", text))
    }

    private fun selectAll() {
        val source = activeSource
        if (source.isEmpty()) return
        selectionStart = SourcePoint(source.first(), 0)
        val last = source.last()
        selectionEnd = SourcePoint(last, last.cells.lastOrNull()?.let { it.column + it.width } ?: 0)
        showSelectionActions()
        invalidate()
    }

    private fun pasteClipboard() {
        if (!inputEnabled) return
        val clip = clipboard.primaryClip ?: return
        val value = buildString {
            for (index in 0 until clip.itemCount) {
                if (index > 0) append('\n')
                append(clip.getItemAt(index).coerceToText(context))
            }
        }
        paste(value)
    }

    private fun paste(value: String) {
        connection?.finishComposingText()
        connection?.resetBuffer()
        val text = value.replace("\r\n", "\n").replace('\r', '\n').replace('\n', '\r')
        if (text.isEmpty()) return
        // Never let a clipboard escape terminate the bracketed-paste envelope early.
        val safe = if (mode("bracketed_paste")) text.replace("\u001b[201~", "") else text
        emit(if (mode("bracketed_paste")) "\u001b[200~$safe\u001b[201~" else safe)
    }

    private fun focusKeyboard() {
        requestFocus()
        inputMethodManager.showSoftInput(this, InputMethodManager.SHOW_IMPLICIT)
    }

    private fun hideKeyboard() {
        inputMethodManager.hideSoftInputFromWindow(windowToken, 0)
    }

    override fun onCheckIsTextEditor() = inputEnabled

    override fun onCreateInputConnection(outAttrs: EditorInfo): InputConnection? {
        if (!inputEnabled || disposed) return null
        outAttrs.inputType = InputType.TYPE_CLASS_TEXT or InputType.TYPE_TEXT_FLAG_MULTI_LINE or InputType.TYPE_TEXT_FLAG_NO_SUGGESTIONS
        outAttrs.imeOptions = EditorInfo.IME_FLAG_NO_EXTRACT_UI or EditorInfo.IME_FLAG_NO_FULLSCREEN or EditorInfo.IME_ACTION_NONE
        outAttrs.initialSelStart = 0
        outAttrs.initialSelEnd = 0
        return TerminalInputConnection().also { connection = it }
    }

    /** The editable is IME context, not the remote screen. Only committed diffs leave this object. */
    private inner class TerminalInputConnection : BaseInputConnection(this@NativeTerminalView, true) {
        private val buffer = Editable.Factory.getInstance().newEditable("")
        private var committed = ""
        private var batchDepth = 0
        private var pendingSync = false
        init { Selection.setSelection(buffer, 0) }
        override fun getEditable(): Editable = buffer
        private fun hasComposition() = getComposingSpanStart(buffer) >= 0 && getComposingSpanEnd(buffer) >= 0
        private fun isCurrent() = connection === this && inputEnabled && !disposed

        fun resetBuffer() {
            removeComposingSpans(buffer)
            buffer.clear()
            Selection.setSelection(buffer, 0)
            committed = ""
            pendingSync = false
            batchDepth = 0
            updateSelection()
        }

        override fun beginBatchEdit(): Boolean {
            if (!isCurrent()) return false
            batchDepth++
            return true
        }
        override fun endBatchEdit(): Boolean {
            if (!isCurrent()) return false
            if (batchDepth > 0) batchDepth--
            if (batchDepth == 0 && pendingSync) syncCommitted()
            updateSelection()
            return true
        }

        override fun setComposingText(text: CharSequence?, newCursorPosition: Int): Boolean {
            if (!isCurrent()) return false
            val result = super.setComposingText(text, newCursorPosition)
            updateSelection()
            return result
        }

        override fun setComposingRegion(start: Int, end: Int): Boolean {
            if (!isCurrent()) return false
            val result = super.setComposingRegion(start, end)
            updateSelection()
            return result
        }

        override fun commitText(text: CharSequence?, newCursorPosition: Int): Boolean {
            if (!isCurrent()) return false
            val result = super.commitText(text, newCursorPosition)
            scheduleSync()
            return result
        }

        override fun finishComposingText(): Boolean {
            if (!isCurrent()) return false
            val result = super.finishComposingText()
            scheduleSync()
            return result
        }

        override fun setSelection(start: Int, end: Int): Boolean {
            if (!isCurrent()) return false
            if (start !in 0..buffer.length || end !in 0..buffer.length) return false
            val result = super.setSelection(start, end)
            updateSelection()
            return result
        }

        override fun deleteSurroundingText(beforeLength: Int, afterLength: Int): Boolean {
            if (!isCurrent() || beforeLength < 0 || afterLength < 0) return false
            val composing = hasComposition()
            val start = min(Selection.getSelectionStart(buffer), Selection.getSelectionEnd(buffer)).coerceAtLeast(0)
            val end = max(Selection.getSelectionStart(buffer), Selection.getSelectionEnd(buffer)).coerceAtLeast(0)
            val missingBefore = if (composing) 0 else max(0, beforeLength - start)
            val missingAfter = if (composing) 0 else max(0, afterLength - (buffer.length - end))
            val result = super.deleteSurroundingText(beforeLength, afterLength)
            scheduleSync()
            if (!composing && missingBefore + missingAfter > 0) {
                // A soft keyboard can delete even when its local context is empty.
                emit("\u007f".repeat(missingBefore.coerceAtMost(65536)) + "\u001b[3~".repeat(missingAfter.coerceAtMost(16384)))
            }
            return result
        }

        override fun deleteSurroundingTextInCodePoints(beforeLength: Int, afterLength: Int): Boolean {
            if (!isCurrent() || beforeLength < 0 || afterLength < 0) return false
            val composing = hasComposition()
            val start = Selection.getSelectionStart(buffer).coerceAtLeast(0)
            val end = Selection.getSelectionEnd(buffer).coerceAtLeast(0)
            val before = min(beforeLength, Character.codePointCount(buffer, 0, start))
            val after = min(afterLength, Character.codePointCount(buffer, end, buffer.length))
            val beforeUnits = start - Character.offsetByCodePoints(buffer, start, -before)
            val afterUnits = Character.offsetByCodePoints(buffer, end, after) - end
            val result = deleteSurroundingText(beforeUnits, afterUnits)
            if (!composing) emit("\u007f".repeat((beforeLength - before).coerceAtMost(65536)) + "\u001b[3~".repeat((afterLength - after).coerceAtMost(16384)))
            return result
        }

        override fun performEditorAction(actionCode: Int): Boolean {
            if (!isCurrent()) return false
            finishComposingText()
            emit("\r")
            resetBuffer()
            return true
        }

        override fun performContextMenuAction(id: Int): Boolean {
            if (!isCurrent()) return false
            return when (id) {
                android.R.id.paste -> { pasteClipboard(); true }
                android.R.id.copy -> { copySelection(); true }
                android.R.id.selectAll -> { selectAll(); true }
                else -> super.performContextMenuAction(id)
            }
        }

        override fun sendKeyEvent(event: KeyEvent): Boolean =
            isCurrent() && this@NativeTerminalView.dispatchKeyEvent(event)

        private fun scheduleSync() {
            pendingSync = true
            if (batchDepth == 0) syncCommitted()
            updateSelection()
        }

        private fun syncCommitted() {
            if (!isCurrent() || hasComposition()) return
            pendingSync = false
            val current = buffer.toString()
            if (current == committed) return
            var prefix = 0
            val shared = min(current.length, committed.length)
            while (prefix < shared && current[prefix] == committed[prefix]) prefix++
            if (prefix > 0 && prefix < committed.length && Character.isLowSurrogate(committed[prefix])) prefix--
            val deletes = committed.codePointCount(prefix, committed.length)
            val insert = current.substring(prefix).replace("\r\n", "\n").replace('\n', '\r')
            emit("\u007f".repeat(deletes) + insert)
            committed = current
            // Bound the candidate context without forgetting which text was already sent.
            if (buffer.length > 1024 && Selection.getSelectionStart(buffer) == buffer.length) {
                var remove = buffer.length - 512
                if (Character.isLowSurrogate(buffer[remove])) remove--
                buffer.delete(0, remove)
                committed = buffer.toString()
            }
        }

        private fun updateSelection() {
            if (!isAttachedToWindow || connection !== this) return
            inputMethodManager.updateSelection(this@NativeTerminalView, Selection.getSelectionStart(buffer), Selection.getSelectionEnd(buffer), getComposingSpanStart(buffer), getComposingSpanEnd(buffer))
        }
    }

    override fun onKeyDown(keyCode: Int, event: KeyEvent): Boolean {
        if (!inputEnabled) return super.onKeyDown(keyCode, event)
        if (event.isCtrlPressed && event.isShiftPressed) {
            if (keyCode == KeyEvent.KEYCODE_C && selectionStart != null) { copySelection(); finishSelection(); return true }
            if (keyCode == KeyEvent.KEYCODE_V) { pasteClipboard(); return true }
        }
        if (keyCode == KeyEvent.KEYCODE_DEL) {
            val editor = connection
            if (editor != null) editor.deleteSurroundingTextInCodePoints(1, 0) else emit("\u007f")
            return true
        }
        val modifier = 1 + (if (event.isShiftPressed) 1 else 0) + (if (event.isAltPressed) 2 else 0) + (if (event.isCtrlPressed) 4 else 0)
        val special = when (keyCode) {
            KeyEvent.KEYCODE_ENTER, KeyEvent.KEYCODE_NUMPAD_ENTER -> if (keyCode == KeyEvent.KEYCODE_NUMPAD_ENTER && mode("app_keypad")) "\u001bOM" else "\r"
            KeyEvent.KEYCODE_ESCAPE -> "\u001b"
            KeyEvent.KEYCODE_TAB -> if (event.isShiftPressed) "\u001b[Z" else "\t"
            KeyEvent.KEYCODE_FORWARD_DEL -> tilde(3, modifier)
            KeyEvent.KEYCODE_DPAD_UP -> arrow('A', modifier)
            KeyEvent.KEYCODE_DPAD_DOWN -> arrow('B', modifier)
            KeyEvent.KEYCODE_DPAD_RIGHT -> arrow('C', modifier)
            KeyEvent.KEYCODE_DPAD_LEFT -> arrow('D', modifier)
            KeyEvent.KEYCODE_MOVE_HOME -> if (modifier == 1) if (mode("app_cursor")) "\u001bOH" else "\u001b[H" else "\u001b[1;${modifier}H"
            KeyEvent.KEYCODE_MOVE_END -> if (modifier == 1) if (mode("app_cursor")) "\u001bOF" else "\u001b[F" else "\u001b[1;${modifier}F"
            KeyEvent.KEYCODE_PAGE_UP -> tilde(5, modifier)
            KeyEvent.KEYCODE_PAGE_DOWN -> tilde(6, modifier)
            KeyEvent.KEYCODE_INSERT -> tilde(2, modifier)
            KeyEvent.KEYCODE_F1, KeyEvent.KEYCODE_F2, KeyEvent.KEYCODE_F3, KeyEvent.KEYCODE_F4 -> {
                val final = ('P'.code + keyCode - KeyEvent.KEYCODE_F1).toChar()
                if (modifier == 1) "\u001bO$final" else "\u001b[1;$modifier$final"
            }
            KeyEvent.KEYCODE_F5 -> tilde(15, modifier)
            KeyEvent.KEYCODE_F6 -> tilde(17, modifier)
            KeyEvent.KEYCODE_F7 -> tilde(18, modifier)
            KeyEvent.KEYCODE_F8 -> tilde(19, modifier)
            KeyEvent.KEYCODE_F9 -> tilde(20, modifier)
            KeyEvent.KEYCODE_F10 -> tilde(21, modifier)
            KeyEvent.KEYCODE_F11 -> tilde(23, modifier)
            KeyEvent.KEYCODE_F12 -> tilde(24, modifier)
            else -> keypadSequence(keyCode)
        }
        if (special != null) {
            connection?.finishComposingText()
            connection?.resetBuffer()
            emit(special)
            return true
        }
        var unicode = event.getUnicodeChar(event.metaState and KeyEvent.META_CTRL_MASK.inv() and KeyEvent.META_ALT_MASK.inv())
        if (unicode == 0) return super.onKeyDown(keyCode, event)
        if (unicode and KeyCharacterMap.COMBINING_ACCENT != 0) {
            deadAccent = unicode and KeyCharacterMap.COMBINING_ACCENT_MASK
            return true
        }
        if (deadAccent != 0) {
            val combined = KeyCharacterMap.getDeadChar(deadAccent, unicode)
            if (combined != 0) unicode = combined
            deadAccent = 0
        }
        var text = String(Character.toChars(unicode))
        if (event.isCtrlPressed) {
            val control = when (unicode) {
                in 0x61..0x7a -> unicode - 0x60
                in 0x40..0x5f -> unicode - 0x40
                0x20, 0x32 -> 0
                0x33 -> 27
                0x34 -> 28
                0x35 -> 29
                0x36 -> 30
                0x37, 0x2f -> 31
                0x38, 0x3f -> 127
                else -> -1
            }
            if (control >= 0) text = control.toChar().toString()
        }
        if (event.isAltPressed) text = "\u001b$text"
        if (event.isCtrlPressed || event.isAltPressed) {
            connection?.finishComposingText()
            connection?.resetBuffer()
            emit(text)
        } else {
            val editor = connection ?: TerminalInputConnection().also { connection = it }
            editor.commitText(text, 1)
        }
        return true
    }

    override fun onKeyUp(keyCode: Int, event: KeyEvent): Boolean {
        if (inputEnabled && (event.unicodeChar != 0 || keyCode in handledKeys)) return true
        return super.onKeyUp(keyCode, event)
    }

    override fun onKeyMultiple(keyCode: Int, repeatCount: Int, event: KeyEvent): Boolean {
        if (inputEnabled && keyCode == KeyEvent.KEYCODE_UNKNOWN && !event.characters.isNullOrEmpty()) {
            val editor = connection ?: TerminalInputConnection().also { connection = it }
            editor.commitText(event.characters, 1)
            return true
        }
        return super.onKeyMultiple(keyCode, repeatCount, event)
    }

    private fun keypadSequence(keyCode: Int): String? {
        if (!mode("app_keypad")) return null
        val code = when (keyCode) {
            in KeyEvent.KEYCODE_NUMPAD_0..KeyEvent.KEYCODE_NUMPAD_9 -> 'p' + keyCode - KeyEvent.KEYCODE_NUMPAD_0
            KeyEvent.KEYCODE_NUMPAD_DOT -> 'n'
            KeyEvent.KEYCODE_NUMPAD_ADD -> 'k'
            KeyEvent.KEYCODE_NUMPAD_SUBTRACT -> 'm'
            KeyEvent.KEYCODE_NUMPAD_MULTIPLY -> 'j'
            KeyEvent.KEYCODE_NUMPAD_DIVIDE -> 'o'
            KeyEvent.KEYCODE_NUMPAD_EQUALS -> 'X'
            KeyEvent.KEYCODE_NUMPAD_COMMA -> 'l'
            else -> return null
        }
        return "\u001bO$code"
    }

    private fun arrow(final: Char, modifier: Int = 1) = if (modifier != 1) "\u001b[1;$modifier$final" else if (mode("app_cursor")) "\u001bO$final" else "\u001b[$final"
    private fun tilde(code: Int, modifier: Int) = if (modifier == 1) "\u001b[$code~" else "\u001b[$code;$modifier~"
    private fun mode(name: String) = frame?.modes?.get(name)?.jsonPrimitive?.booleanOrNull == true
    private fun JsonObject.number(name: String) = get(name)?.jsonPrimitive?.intOrNull ?: 0
    private fun TerminalFrame.flagCursor(name: String) = cursor[name]?.jsonPrimitive?.booleanOrNull == true

    /** Each RPC stays inside the protocol's UTF-8 input ceiling, including supplementary characters. */
    private fun emit(text: String, returnToBottom: Boolean = true, resetMotion: Boolean = true) {
        if (!inputEnabled || disposed || text.isEmpty()) return
        if (resetMotion) stopMotion()
        if (returnToBottom && !historyVisible && !historyOpening && following) scrollYpx = maxScroll(liveLayout)
        var first = 0
        var index = 0
        var bytes = 0
        while (index < text.length) {
            val codePoint = text.codePointAt(index)
            val size = when {
                codePoint < 0x80 -> 1
                codePoint < 0x800 -> 2
                codePoint < 0x10000 -> 3
                else -> 4
            }
            if (bytes + size > 65536) {
                onInput(text.substring(first, index))
                first = index
                bytes = 0
            }
            bytes += size
            index += Character.charCount(codePoint)
        }
        if (first < text.length) onInput(if (first == 0) text else text.substring(first))
    }

    override fun onFocusChanged(gainFocus: Boolean, direction: Int, previouslyFocusedRect: Rect?) {
        super.onFocusChanged(gainFocus, direction, previouslyFocusedRect)
        if (!gainFocus) {
            // Focus loss must not accidentally commit a candidate which the user never accepted.
            connection?.resetBuffer()
            deadAccent = 0
        }
        cursorOn = true
        invalidate()
    }

    override fun onAttachedToWindow() {
        super.onAttachedToWindow()
        if (frame?.flagCursor("blink") == true) postDelayed(blink, 550)
    }

    override fun onDetachedFromWindow() {
        stopMotion()
        removeCallbacks(blink)
        finishSelection()
        velocity?.recycle(); velocity = null
        connection?.resetBuffer()
        connection = null
        hideKeyboard()
        super.onDetachedFromWindow()
    }

    private fun accessibleText(): String {
        val revision = (if (historyVisible) "history:${historySource.size}:$historyCursor" else frame?.revision.orEmpty()) + ":${scrollYpx.toInt()}:$historyVisible"
        if (accessibilityRevision == revision) return accessibilitySnapshot
        val first = max(0, floor(scrollYpx / cellHeight).toInt())
        val last = min(activeLayout.lastIndex, ceil((scrollYpx + viewportHeight) / cellHeight).toInt())
        accessibilitySnapshot = buildString {
            if (historyVisible) append("主屏历史，只读。") else if (frame?.alternate == true) append("应用屏幕。")
            for (index in first..last) {
                var column = 0
                for (placed in activeLayout[index].cells) {
                    while (column < placed.column) { append(' '); column++ }
                    if (!placed.cell.style.hidden) append(placed.cell.text)
                    column = placed.column + placed.cell.width
                }
                append('\n')
            }
            if (historyLoading) append("正在加载历史。")
            historyError?.let { append("历史加载失败：").append(it).append('。') }
            if (newerBoundary) append("较新内容已移出缓存，可重新加载。")
            if (historyTruncated) append("更早历史已不再保留。")
        }
        accessibilityRevision = revision
        return accessibilitySnapshot
    }

    override fun onInitializeAccessibilityNodeInfo(info: AccessibilityNodeInfo) {
        super.onInitializeAccessibilityNodeInfo(info)
        info.className = "android.widget.EditText"
        info.text = accessibleText()
        info.isEditable = inputEnabled && !historyVisible
        info.isMultiLine = true
        info.isScrollable = true
        info.addAction(AccessibilityNodeInfo.AccessibilityAction.ACTION_SCROLL_BACKWARD)
        info.addAction(AccessibilityNodeInfo.AccessibilityAction.ACTION_SCROLL_FORWARD)
        info.addAction(AccessibilityNodeInfo.AccessibilityAction.ACTION_CLICK)
        info.addAction(AccessibilityNodeInfo.AccessibilityAction.ACTION_LONG_CLICK)
        info.addAction(AccessibilityNodeInfo.AccessibilityAction(ACCESS_HISTORY, "打开主屏历史"))
        info.addAction(AccessibilityNodeInfo.AccessibilityAction(ACCESS_LIVE, "返回实时"))
        info.addAction(AccessibilityNodeInfo.AccessibilityAction(ACCESS_RELOAD, "重新加载历史"))
        if (selectionStart != null) info.addAction(AccessibilityNodeInfo.AccessibilityAction.ACTION_COPY)
        if (inputEnabled) {
            info.addAction(AccessibilityNodeInfo.AccessibilityAction.ACTION_PASTE)
            info.addAction(AccessibilityNodeInfo.AccessibilityAction.ACTION_SET_TEXT)
        }
    }

    override fun performAccessibilityAction(action: Int, arguments: Bundle?): Boolean {
        when (action) {
            AccessibilityNodeInfo.ACTION_SCROLL_BACKWARD -> scrollAmount(-max(cellHeight, viewportHeight * .8f))
            AccessibilityNodeInfo.ACTION_SCROLL_FORWARD -> scrollAmount(max(cellHeight, viewportHeight * .8f))
            AccessibilityNodeInfo.ACTION_CLICK -> if (inputEnabled) focusKeyboard() else return false
            AccessibilityNodeInfo.ACTION_LONG_CLICK -> { selectAll(); showSelectionActions() }
            AccessibilityNodeInfo.ACTION_COPY -> copySelection()
            AccessibilityNodeInfo.ACTION_PASTE -> pasteClipboard()
            AccessibilityNodeInfo.ACTION_SET_TEXT -> {
                val text = arguments?.getCharSequence(AccessibilityNodeInfo.ACTION_ARGUMENT_SET_TEXT_CHARSEQUENCE) ?: return false
                paste(text.toString())
            }
            ACCESS_HISTORY -> openHistory()
            ACCESS_LIVE -> returnLive()
            ACCESS_RELOAD -> openHistory(reload = true)
            else -> return super.performAccessibilityAction(action, arguments)
        }
        sendAccessibilityEvent(AccessibilityEvent.TYPE_VIEW_SCROLLED)
        return true
    }

    private fun mouseSequence(button: Int, col: Int, row: Int, release: Boolean): String = if (mode("sgr_mouse")) {
        "\u001b[<$button;$col;$row${if (release) 'm' else 'M'}"
    } else "\u001b[M${(if (release) 3 + 32 else button + 32).toChar()}${(min(col, 223) + 32).toChar()}${(min(row, 223) + 32).toChar()}"

    private companion object {
        const val ACCESS_HISTORY = 0x01070001
        const val ACCESS_LIVE = 0x01070002
        const val ACCESS_RELOAD = 0x01070003
        val handledKeys = setOf(KeyEvent.KEYCODE_DEL, KeyEvent.KEYCODE_FORWARD_DEL, KeyEvent.KEYCODE_ESCAPE,
            KeyEvent.KEYCODE_ENTER, KeyEvent.KEYCODE_TAB, KeyEvent.KEYCODE_DPAD_UP, KeyEvent.KEYCODE_DPAD_DOWN,
            KeyEvent.KEYCODE_DPAD_LEFT, KeyEvent.KEYCODE_DPAD_RIGHT, KeyEvent.KEYCODE_MOVE_HOME, KeyEvent.KEYCODE_MOVE_END,
            KeyEvent.KEYCODE_PAGE_UP, KeyEvent.KEYCODE_PAGE_DOWN, KeyEvent.KEYCODE_INSERT)
    }
}
