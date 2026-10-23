'use strict';

let generation = 0;
function fail(message) {
    if (window.Tty7) window.Tty7.failed(generation, message);
}
window.addEventListener('error', event => fail(event.message || '终端脚本执行失败'));
window.addEventListener('unhandledrejection', () => fail('终端渲染任务失败'));

const viewport = document.getElementById('viewport');
const history = document.getElementById('history');
const historyText = document.getElementById('history-text');
const terminalFontFamily = getComputedStyle(document.documentElement).getPropertyValue('--terminal-font').trim();
// Finish loading both weights and symbol fallbacks before reporting ready or
// measuring the phone grid. The first fallback measurement otherwise persists.
const terminalFontsReady = Promise.all([
    ['14px "JetBrainsMono Nerd Font Mono"', 'M\ue0b0\uf07b\udb80\udc01'],
    ['bold 14px "JetBrainsMono Nerd Font Mono"', 'M\ue0b0'],
    ['14px "Noto Sans Symbols"', '\u23bf'],
    ['14px "Noto Sans Symbols 2"', '\u23fa'],
    ['14px "Noto Emoji"', '\u2705'],
].map(async ([font, sample]) => {
    if (!(await document.fonts.load(font, sample)).length) throw new Error('终端字体加载失败');
}));
const systemTheme = matchMedia('(prefers-color-scheme: dark)');
const initialTheme = new URLSearchParams(location.search).get('theme');
let nativeDark = initialTheme === 'dark' ? true : initialTheme === 'light' ? false : null;
if (nativeDark !== null) document.documentElement.dataset.theme = nativeDark ? 'dark' : 'light';
const ansiPalettes = {
    light:['#24292e','#d1242f','#1a7f37','#9a6700','#0969da','#8250df','#1b7c83','#6e7781',
        '#57606a','#cf222e','#1f883d','#bf8700','#218bff','#a475f9','#3192aa','#8c959f'],
    dark:['#616161','#ff8272','#b4fa72','#fefdc2','#a5d5fe','#ff8ffd','#d0d1fe','#f1f1f1',
        '#8e8e8e','#ffc4bd','#d6fcb9','#fefdd5','#c1e3fe','#ffb1fe','#e5e6fe','#feffff']
};
function ansiPalette() { return ansiPalettes[(nativeDark ?? systemTheme.matches) ? 'dark' : 'light']; }
function terminalTheme() {
    const dark = nativeDark ?? systemTheme.matches;
    const theme = dark
        ? {background:'#1e1e20', foreground:'#ececed', cursor:'#ececed', cursorAccent:'#1e1e20'}
        : {background:'#fcfcfb', foreground:'#1c1c1e', cursor:'#1c1c1e', cursorAccent:'#fcfcfb'};
    const names = ['black','red','green','yellow','blue','magenta','cyan','white'];
    const palette = ansiPalette();
    names.forEach((name, index) => {
        theme[name] = palette[index];
        theme['bright' + name[0].toUpperCase() + name.slice(1)] = palette[index + 8];
    });
    return theme;
}
const terminal = new Terminal({
    cols:80, rows:24, fontSize:14, fontFamily:'monospace', scrollback:0,
    convertEol:false, allowProposedApi:false, altClickMovesCursor:false,
    cursorBlink:true, cursorInactiveStyle:'block',
    theme:terminalTheme(),
    // Desktop applications can emit dark ANSI/truecolour text for a light theme.
    // Keep their colours where readable, correcting contrast against each cell.
    minimumContrastRatio:4.5
});
systemTheme.addEventListener('change', applyAppearance);
// Android WebView's media query can follow the Activity theme instead of the
// selected app appearance. The native surface supplies the definitive value.
window.tty7Appearance = function(dark) {
    nativeDark = dark;
    document.documentElement.dataset.theme = dark ? 'dark' : 'light';
    applyAppearance();
};
function applyAppearance() {
    terminal.options.theme = terminalTheme();
    // History is a styled DOM cache; recolour it without clearing its contents
    // or requesting remote history again when the user changes themes.
    for (const span of historyText.querySelectorAll('span[data-fg]')) {
        const bg = resolveColor(span.dataset.bg);
        span.style.color = readable(resolveColor(span.dataset.fg), bg);
        span.style.backgroundColor = bg;
    }
}
function installCanonicalWidths(target) {
    const provider = new window.Tty7ScreenWidths();
    // xterm exposes Unicode providers through its documented proposed API.
    target.options.allowProposedApi = true;
    target.unicode.register(provider);
    target.unicode.activeVersion = provider.version;
    provider.bind(target);
    target.options.allowProposedApi = false;
    return provider;
}
const liveWidths = installCanonicalWidths(terminal);
terminal.open(document.getElementById('terminal'));
const screen = terminal.element.querySelector('.xterm-screen');
const scrollback = terminal.element.querySelector('.xterm-viewport');
const input = terminal.element.querySelector('.xterm-helper-textarea');
// Android can reopen the IME when a gesture lands on a still-focused textarea.
// Keep it read-only until the gesture has ended as an intentional tap.
input.readOnly = true;
function blurInput() {
    input.readOnly = true;
    terminal.blur();
}

let composingInput = false;
let compositionPending = false;
let imeValue = null;
let imeTimer = 0;
function flushImeInput() {
    if (imeValue === null || composingInput || compositionPending) return;
    const previous = Array.from(imeValue);
    const current = Array.from(input.value);
    imeValue = input.value;
    if (input.readOnly || !inputReady) return;
    let common = 0;
    while (common < previous.length && common < current.length && previous[common] === current[common]) common++;
    const data = '\x7f'.repeat(previous.length - common) + current.slice(common).join('').replace(/\r?\n/g, '\r');
    if (data) terminal.input(data, true);
}
function resetImeInput() {
    clearTimeout(imeTimer);
    imeTimer = 0;
    imeValue = null;
}
// xterm's 229 fallback queues one textarea snapshot per keydown. If several
// edits arrive before those timers run, their overlapping diffs replay text;
// an equal-length autocorrection also sends the entire old textarea again.
// Own only this fallback, consuming each DOM edit once. xterm still owns
// composition candidates, hardware keys and paste.
terminal.attachCustomKeyEventHandler(event => {
    if (event.type !== 'keydown') return true;
    if (event.keyCode !== 229) {
        flushImeInput();
        resetImeInput();
        return true;
    }
    if (!composingInput && !compositionPending) {
        flushImeInput();
        imeValue = input.value;
        clearTimeout(imeTimer);
        imeTimer = setTimeout(() => { imeTimer = 0; flushImeInput(); }, 0);
    }
    return false;
});
// Capture above xterm's input listener so its insertText fallback cannot send
// the same edit a second time, including events without a composed flag.
terminal.element.addEventListener('input', event => {
    if (event.target !== input) return;
    if (composingInput || compositionPending || event.isComposing) {
        event.stopImmediatePropagation();
        return;
    }
    if (imeValue === null) return;
    event.stopImmediatePropagation();
    flushImeInput();
}, {capture:true});
terminal.element.addEventListener('paste', () => {
    flushImeInput();
    resetImeInput(); // xterm sends the paste and clears its textarea itself.
}, {capture:true});
let backspaceKeyActive = false;
let backspaceKeyHandled = false;
// Observe on the ancestor before xterm's textarea keydown listener runs.
terminal.element.addEventListener('keydown', event => {
    backspaceKeyActive = event.key === 'Backspace' || event.keyCode === 8;
    backspaceKeyHandled = false;
}, {capture:true});
input.addEventListener('keyup', () => {
    backspaceKeyActive = backspaceKeyHandled = false;
    flushImeInput();
    resetImeInput();
});
input.addEventListener('blur', () => {
    backspaceKeyActive = backspaceKeyHandled = false;
    resetImeInput();
});
input.addEventListener('compositionstart', () => {
    flushImeInput();
    resetImeInput();
    composingInput = true;
});
input.addEventListener('compositionend', () => {
    composingInput = false;
    // xterm commits after this event and includes any following ordinary text
    // in the same turn. Let that commit finish before owning new 229 edits.
    compositionPending = true;
    setTimeout(() => { compositionPending = false; }, 0);
});
input.addEventListener('beforeinput', event => {
    // Android may send only keyCode 229 plus deleteContentBackward. xterm
    // detects 229 deletion by a textarea length change, which cannot happen
    // when its local IME buffer is empty but the remote editor still has text.
    // Nonempty buffers follow the text-edit path; candidate deletion stays
    // with xterm. Cancel the empty edit so no second path can emit DEL.
    if (event.inputType !== 'deleteContentBackward' || !event.cancelable ||
        event.isComposing || composingInput || input.value !== '' ||
        input.readOnly || !inputReady) return;
    event.preventDefault();
    event.stopImmediatePropagation();
    if (!backspaceKeyHandled) terminal.input('\x7f', true);
}, {capture:true});

// This is an observer: the desktop terminal, not a replay on the phone, owns
// protocol replies. Keep keyboard/IME/paste input, but never answer PTY queries.
for (const id of [
    {final:'c'}, {prefix:'>',final:'c'}, {final:'n'}, {prefix:'?',final:'n'},
    {intermediates:'$',final:'p'}, {prefix:'?',intermediates:'$',final:'p'}
]) terminal.parser.registerCsiHandler(id, () => true);
terminal.parser.registerCsiHandler({final:'t'}, params => [14,16,18].includes(params[0]));
terminal.parser.registerDcsHandler({intermediates:'$',final:'q'}, () => true);
for (const id of [4,10,11,12]) {
    terminal.parser.registerOscHandler(id, data => data.split(';').includes('?'));
}
terminal.onData(data => {
    if (data === '\x1b[I' || data === '\x1b[O') return; // Local focus is not desktop focus.
    if (backspaceKeyActive && data === '\x7f') backspaceKeyHandled = true;
    stopScrolling();
    if (inputReady && window.Tty7) window.Tty7.input(generation, data);
});

let following = true;
let inputReady = false;
let fontsReady = false;
let sizeTimer;
let sentGrid = '';
let applicationScreen = null;
let applicationScrollDebt = 0;
let applicationScrollTimer = 0;
// Pace wheel batches by their input RPC replies, never by screen revisions:
// terminal output has no causal link to a particular input. Bound outstanding
// rows instead of serializing every batch behind a network round trip.
const maxApplicationScrollBatch = 20;
const minApplicationScrollInterval = 32;
const bulkApplicationScrollInterval = 16;
const maxApplicationScrollOutstandingRows = 80;
const applicationScrollReverseReserve = 20;
const maxApplicationScrollOutstandingBatches = 16;
const maxApplicationScrollDebt = 200;
const maxApplicationScrollLeadRows = 3;
let applicationScrollSentRows = 0;
let applicationScrollSentSign = 0;
let applicationScrollSentAt = 0;
let applicationScrollLastBatchRows = 0;
let applicationScrollInFlight = false;
let applicationScrollStalled = false;
let applicationScrollAckEwma = 0;
let applicationScrollTimeouts = 0;
let applicationScrollBatchId = 0;
const applicationScrollBatches = new Map();
let applicationScrollSendTimer = 0;
let visualOffset = 0;
let visualSettle = 0;
let surfaceHeight = '';
let surfaceWidth = '';
let surfaceRows = 0;
let surfaceCols = 0;
let cellSize = {width:8, height:16};
let scrollOrigin = {left:0, top:0};
let scrollPoint = {x:0, y:0};
function requestPhoneGrid() {
    if (!fontsReady || sizeTimer) return;
    sizeTimer = setTimeout(() => {
        sizeTimer = 0;
        if (!fontsReady || !screen.clientWidth || !screen.clientHeight) return;
        const cellWidth = screen.clientWidth / terminal.cols;
        const cellHeight = screen.clientHeight / terminal.rows;
        // Leave room for the terminal's own vertical scrollbar and page padding.
        const cols = Math.max(20, Math.min(1000, Math.floor((viewport.clientWidth - 20) / cellWidth)));
        const rows = Math.max(5, Math.min(1000, Math.floor((viewport.clientHeight - 4) / cellHeight)));
        const grid = `${cols}x${rows}`;
        if (grid === sentGrid) return;
        sentGrid = grid;
        if (window.Tty7) window.Tty7.resize(cols, rows);
    }, 120);
}
window.tty7Synced = function(gen) {
    if (gen !== generation) return;
    inputReady = true;
    sentGrid = '';
    requestPhoneGrid();
};
function sizeSurface() {
    // Grid changes come back in ordered PTY frames, never reflow ahead of output.
    const height = screen.style.height;
    const width = screen.style.width;
    if (surfaceHeight === height && surfaceWidth === width && surfaceRows === terminal.rows && surfaceCols === terminal.cols) return;
    surfaceHeight = height;
    surfaceWidth = width;
    surfaceRows = terminal.rows;
    surfaceCols = terminal.cols;
    // xterm already calculated these pixel dimensions. Reading inline styles
    // avoids forcing DOM layout in every remote gesture after a screen patch.
    cellSize = {width:parseFloat(width) / terminal.cols || 8, height:parseFloat(height) / terminal.rows || 16};
    // Include the mount's 2px top/bottom padding. The mount clips the screen
    // so a local scroll lead cannot extend the viewport's scrollable area, and
    // the surface must not lose its last rows to that clipping at rest.
    const heightPx = parseFloat(height);
    terminal.element.style.height = Number.isFinite(heightPx) ? `${heightPx + 4}px` : height;
    if (visualOffset || applicationScrollSentRows) updateVisualOffset();
    requestPhoneGrid();
    if (following) returnLive();
}
function isAtLive() {
    return !historyOpening && history.hidden &&
        // DOM scrollTop changes immediately; xterm's viewportY catches up later.
        scrollback.scrollTop >= scrollback.scrollHeight - scrollback.clientHeight - 1 &&
        viewport.scrollTop >= viewport.scrollHeight - viewport.clientHeight - 1;
}
function returnLive() {
    history.hidden = true;
    historyOpening = false;
    cancelAnimationFrame(coast);
    coast = 0;
    setVisualOffset(0);
    if (touch?.axis === 'y') touch.blocked = true;
    historyPendingScroll = 0;
    terminal.scrollToBottom();
    scrollback.scrollTop = scrollback.scrollHeight;
    viewport.scrollTop = viewport.scrollHeight;
    following = true;
    updateHistoryLive();
}
window.tty7Key = function(gen, text) {
    if (gen !== generation || !inputReady) return;
    stopScrolling();
    if (window.Tty7) window.Tty7.input(generation, text);
};
viewport.addEventListener('scroll', () => {
    following = isAtLive();
}, {passive:true});
terminal.onRender(() => {
    // WebView may measure fonts after the first write/attachment.
    sizeSurface();
});

const historyStatus = document.getElementById('history-status');
const historyLive = document.getElementById('history-live');
const historyBoundary = document.getElementById('history-boundary');
let historyAgentKind = null;
window.tty7AgentKind = function(gen, kind) {
    if (gen !== generation) return;
    historyAgentKind = kind;
    updateHistoryLive();
};
for (const button of [historyLive, document.getElementById('history-newer')]) {
    button.addEventListener('pointerdown', event => event.preventDefault());
    button.addEventListener('click', event => {
        event.preventDefault();
        stopScrolling();
        blurInput();
        if (button === historyLive) returnLive();
        else if (!historyLoading) { resetHistory(); showHistory(); }
    });
}
const historyParser = new Terminal({cols:80, rows:1, scrollback:0, allowProposedApi:false});
const historyWidths = installCanonicalWidths(historyParser);
let historyCursor = null;
let historyMore = true;
let historyLoading = false;
let historyLoaded = false;
let historyOpening = false;
let historyBytes = 0;
let historyTruncated = false;
let historyError = null;
let historyScrollTop = 0;
let historyPendingScroll = 0;
let historyRevision = null;
let historyNewerBoundary = false;
function resetHistory() {
    historyText.replaceChildren();
    historyCursor = null; historyMore = true; historyLoaded = false; historyBytes = 0;
    historyTruncated = false; historyError = null; historyScrollTop = 0;
    historyPendingScroll = 0;
    historyOpening = false;
    historyRevision = null; historyNewerBoundary = false;
    historyBoundary.hidden = true;
}
// A separate read-only styled cache, at most 2,000 rows / 2 MiB UTF-8.
const maxHistoryRows = 2000;
const maxHistoryBytes = 2 * 1024 * 1024;
function updateHistoryLive() {
    historyLive.hidden = history.hidden || historyAgentKind === 'codex' ||
        (!historyError && !historyNewerBoundary && isAtHistoryBottom());
}
function updateHistoryStatus() {
    historyStatus.textContent = historyLoading ? '正在加载历史…' :
        historyError ? historyError + '；返回实时后重试' :
        !historyMore && history.scrollTop <= 1 ? (historyTruncated ? '已到保留历史边界（更早内容已不再保留）' : '已到历史起点') :
        historyNewerBoundary ? '连续历史窗口（较新内容可重新加载）' :
        historyMore ? '向上滚动加载更多' : '主屏幕历史（只读）';
    updateHistoryLive();
}
function isAtHistoryBottom() {
    return history.scrollTop >= history.scrollHeight - history.clientHeight - 1;
}
function requestHistory() {
    if (!inputReady || historyLoading || !historyMore) return;
    historyLoading = true;
    if (!historyLoaded) historyRevision = appliedRevision;
    historyError = null;
    updateHistoryStatus();
    if (window.Tty7) window.Tty7.history(generation, historyCursor || '');
}
function showHistory() {
    // Reopening must not attach today's screen to yesterday's cached suffix.
    // A pending response owns its snapshot until it completes; do not overlap
    // requests, whose native callbacks are generation-scoped rather than tagged.
    if (!historyLoading && (historyRevision !== appliedRevision || historyNewerBoundary || historyError)) resetHistory();
    if (historyLoaded && !historyMore && !historyText.childElementCount) return false;
    following = false;
    // Keep the live renderer visible until the immutable page is fully parsed.
    // A placeholder has different wrapping/geometry and flashes at the boundary.
    if (!historyLoaded) {
        historyOpening = true;
        blurInput();
        historyMore = true;
        requestHistory();
        return true;
    }
    history.hidden = false;
    history.scrollLeft = viewport.scrollLeft;
    history.scrollTop = history.scrollHeight;
    // Opening/prepending history also emits scroll events. Only a downward
    // movement by the reader should leave history at its lower boundary.
    historyScrollTop = history.scrollTop;
    updateHistoryStatus();
    blurInput();
    return true;
}
history.addEventListener('scroll', () => {
    if (history.hidden || historyOpening) return;
    const movedDown = history.scrollTop > historyScrollTop;
    historyScrollTop = history.scrollTop;
    if (movedDown && isAtHistoryBottom() && !historyNewerBoundary && !historyLoading) { returnLive(); return; }
    updateHistoryStatus();
    if (history.scrollTop < 160) requestHistory();
});
function cellColor(cell, foreground) {
    const value = foreground ? cell.getFgColor() : cell.getBgColor();
    if (foreground ? cell.isFgRGB() : cell.isBgRGB()) return '#' + value.toString(16).padStart(6, '0');
    if (!(foreground ? cell.isFgPalette() : cell.isBgPalette())) return foreground ? 'foreground' : 'background';
    return 'p' + value;
}
function resolveColor(token) {
    if (token.startsWith('#')) return token;
    if (token === 'foreground' || token === 'background') return terminalTheme()[token];
    const value = Number(token.slice(1));
    if (value < 16) return ansiPalette()[value];
    if (value >= 232) { const v = (8 + (value - 232) * 10).toString(16).padStart(2, '0'); return '#' + v.repeat(3); }
    const index = value - 16, channel = n => (n ? 55 + n * 40 : 0).toString(16).padStart(2, '0');
    return '#' + channel(Math.floor(index / 36)) + channel(Math.floor(index / 6) % 6) + channel(index % 6);
}
function luminance(hex) {
    const channels = [1,3,5].map(i => parseInt(hex.slice(i, i + 2), 16) / 255)
        .map(v => v <= 0.04045 ? v / 12.92 : Math.pow((v + 0.055) / 1.055, 2.4));
    return channels[0] * 0.2126 + channels[1] * 0.7152 + channels[2] * 0.0722;
}
function readable(fg, bg) {
    const f = luminance(fg), b = luminance(bg);
    if ((Math.max(f,b) + 0.05) / (Math.min(f,b) + 0.05) >= 4.5) return fg;
    return (b + 0.05) / 0.05 >= 4.5 ? '#000000' : '#ffffff';
}
function historyRow(line, rowIndex, source = historyParser) {
    const row = document.createElement('div');
    row.style.minHeight = '1em';
    row.dataset.bytes = String(new TextEncoder().encode(line.ansi + line.text).length);
    const cells = source.buffer.active.getLine(rowIndex);
    let run = null, previous = '';
    for (let col = 0; col < line.cols; ++col) {
        const cell = cells.getCell(col);
        if (!cell || cell.getWidth() === 0) continue;
        let fgToken = cellColor(cell, true), bgToken = cellColor(cell, false);
        if (cell.isInverse()) [fgToken, bgToken] = [bgToken, fgToken];
        const fg = resolveColor(fgToken), bg = resolveColor(bgToken);
        const style = `color:${readable(fg,bg)};background:${bg};font-weight:${cell.isBold() ? 'bold' : 'normal'};font-style:${cell.isItalic() ? 'italic' : 'normal'};text-decoration:${cell.isUnderline() ? 'underline' : ''} ${cell.isStrikethrough() ? 'line-through' : ''}`;
        const chars = cell.isInvisible() ? ' '.repeat(cell.getWidth()) : cell.getChars() || ' ';
        const positioned = /[^\x20-\x7e]/.test(chars) || cell.getWidth() !== 1;
        const runKey = `${fgToken}|${bgToken}|${style}`;
        if (positioned || runKey !== previous || !run) {
            run = document.createElement('span');
            run.style.cssText = style;
            run.dataset.fg = fgToken;
            run.dataset.bg = bgToken;
            if (positioned) {
                run.style.display = 'inline-block';
                run.style.width = `${cell.getWidth()}ch`;
                run.style.overflow = 'hidden';
                run.style.verticalAlign = 'top';
            }
            row.appendChild(run);
            previous = runKey;
        }
        run.appendChild(document.createTextNode(chars));
        if (positioned) run = null;
    }
    return row;
}
window.tty7History = async function(gen, page) {
    if (gen !== generation || !historyLoading) return;
    const firstPage = !historyLoaded;
    try {
        if (page.lines.length) {
            // Parse a bounded page in one write. Per-row asynchronous writes
            // otherwise pay WebView timer latency hundreds of times.
            await new Promise(resolve => historyParser.write('', resolve));
            if (gen !== generation) return;
            historyParser.reset();
            const cols = page.lines.reduce((width, line) => Math.max(width, line.cols), 1);
            historyParser.resize(cols, page.lines.length);
            historyWidths.prepare(page.lines, cols);
            let paint = '\x1b[?7l';
            for (let index = 0; index < page.lines.length; ++index) {
                paint += `\x1b[${index + 1};1H\x1b[2K${page.lines[index].ansi}\x1b[0m`;
            }
            await new Promise(resolve => historyParser.write(paint, resolve));
            if (gen !== generation) return;
        }
        const fragment = document.createDocumentFragment();
        for (let index = 0; index < page.lines.length; ++index) {
            const line = page.lines[index];
            const row = historyRow(line, index);
            historyBytes += Number(row.dataset.bytes);
            fragment.appendChild(row);
        }
        const first = historyText.firstElementChild;
        const anchorTop = first ? first.getBoundingClientRect().top : 0;
        historyText.prepend(fragment);
        if (first) history.scrollTop += first.getBoundingClientRect().top - anchorTop;
        let evicted = false;
        while (historyText.childElementCount > maxHistoryRows || historyBytes > maxHistoryBytes) {
            const last = historyText.lastElementChild;
            historyBytes -= Number(last.dataset.bytes);
            last.remove();
            evicted = true;
        }
        if (evicted) {
            historyNewerBoundary = true;
            historyBoundary.hidden = false;
        }
        if (firstPage && historyOpening && historyText.childElementCount) {
            history.hidden = false;
            history.scrollLeft = viewport.scrollLeft;
            historyOpening = false;
        }
        if (!first) history.scrollTop = Math.max(0, history.scrollHeight - history.clientHeight + historyPendingScroll);
        historyPendingScroll = 0;
        historyCursor = page.cursor;
        historyMore = page.has_more;
        historyLoaded = true;
        historyLoading = false;
        historyTruncated = historyTruncated || page.truncated;
        historyScrollTop = history.scrollTop;
        updateHistoryStatus();
        if (!historyMore && !historyText.childElementCount) returnLive();
    } catch (error) { window.tty7HistoryError(gen, error.message || '历史加载失败'); }
};
window.tty7HistoryError = function(gen, message) {
    if (gen !== generation) return;
    if (historyOpening) { historyOpening = false; history.hidden = false; }
    historyLoading = false;
    historyError = message;
    historyMore = false;
    historyCursor = null;
    // Keep the already rendered, coherent window available on page failure.
    // The next reopening starts a fresh snapshot because cursors are single-use.
    historyScrollTop = history.scrollTop;
    updateHistoryStatus();
};

function moveScroll(element, amount) {
    const previous = element.scrollTop;
    element.scrollTop += amount;
    return amount - (element.scrollTop - previous);
}
function applicationScrolls() {
    const modes = applicationScreen?.modes;
    return inputReady && history.hidden && applicationScreen?.alternate && (modes.mouse || modes.alternate_scroll);
}
function applicationScrollRowHeight() {
    return Math.max(1, cellSize.height);
}
function applicationScrollDirection() {
    if (applicationScrollDebt) return Math.sign(applicationScrollDebt);
    return applicationScrollSentRows ? applicationScrollSentSign : 0;
}
/// How far the rendered screen may run ahead of the application, in pixels.
/// Only whole rows in queued or undelivered input count; beyond three rows the
/// lead is held and the exposed edge is the pending strip.
function applicationScrollTarget() {
    if (applicationScrollStalled) return 0;
    const direction = applicationScrollDirection();
    if (!direction) return 0;
    // Whole rows only: a sub-row remainder is never sent and must not leave a
    // permanent fractional lead when the gesture ends.
    const unsent = applicationScrollDebt * direction > 0 ? Math.floor(Math.abs(applicationScrollDebt)) : 0;
    const sent = applicationScrollSentSign === direction ? applicationScrollSentRows : 0;
    const rows = Math.min(maxApplicationScrollLeadRows, unsent + sent);
    return -direction * rows * applicationScrollRowHeight();
}
function setVisualOffset(offset) {
    if (visualSettle) { cancelAnimationFrame(visualSettle); visualSettle = 0; }
    visualOffset = offset;
    // The screen, not its ancestors: the IME target stays fixed and the
    // viewport's scrollHeight is not extended by a transformed container.
    screen.style.transform = offset ? `translate3d(0,${offset.toFixed(2)}px,0)` : '';
    screen.style.willChange = offset ? 'transform' : '';
}
function animateVisualOffset(target) {
    if (visualSettle) { cancelAnimationFrame(visualSettle); visualSettle = 0; }
    if (Math.abs(visualOffset - target) < 0.25) { setVisualOffset(target); return; }
    const from = visualOffset;
    const started = performance.now();
    const step = now => {
        const progress = Math.min(1, (now - started) / 120);
        const eased = 1 - Math.pow(1 - progress, 3);
        setVisualOffset(from + (target - from) * eased);
        if (progress < 1) visualSettle = requestAnimationFrame(step);
        else { visualSettle = 0; setVisualOffset(target); }
    };
    visualSettle = requestAnimationFrame(step);
}
function updateVisualOffset() {
    setVisualOffset(applicationScrollTarget());
}
function applicationScrollOutstandingRows() {
    let rows = 0;
    for (const batch of applicationScrollBatches.values()) rows += batch.rows;
    return rows;
}
function applicationScrollCredit() {
    // Canceled movement still occupies transport capacity until its real reply.
    // Reserve one batch for a fresh touch/reversal without growing the window on
    // every reversal. All outstanding rows, including obsolete ones, are counted.
    const obsolete = [...applicationScrollBatches.values()].some(batch => batch.obsolete);
    return Math.max(0, maxApplicationScrollOutstandingRows +
        (obsolete ? applicationScrollReverseReserve : 0) - applicationScrollOutstandingRows());
}
function refreshApplicationScroll() {
    applicationScrollSentRows = 0;
    for (const batch of applicationScrollBatches.values()) {
        if (!batch.obsolete) applicationScrollSentRows += batch.rows;
    }
    applicationScrollInFlight = applicationScrollSentRows > 0;
}
function cancelApplicationScroll() {
    applicationScrollDebt = 0;
    for (const batch of applicationScrollBatches.values()) batch.obsolete = true;
    applicationScrollSentRows = 0;
    applicationScrollSentSign = 0;
    applicationScrollSentAt = 0;
    applicationScrollLastBatchRows = 0;
    applicationScrollInFlight = false;
    applicationScrollStalled = false;
    applicationScrollTimeouts = 0;
    clearTimeout(applicationScrollTimer);
    clearTimeout(applicationScrollSendTimer);
    applicationScrollTimer = applicationScrollSendTimer = 0;
    setVisualOffset(0);
}
function applicationScrollTimeout() {
    // The row window already bounds traffic. Leave at least 500ms for a reply,
    // including when a previously fast mobile link becomes slower mid-gesture.
    const measured = applicationScrollAckEwma || 200;
    const base = Math.max(500, Math.min(1200, Math.round(measured * 2.5)));
    return Math.min(1500, Math.round(base * Math.min(4, 1 + applicationScrollTimeouts * 0.5)));
}
function armApplicationScrollTimeout() {
    clearTimeout(applicationScrollTimer);
    applicationScrollTimer = 0;
    if (applicationScrollStalled) return;
    let deadline = Infinity;
    for (const batch of applicationScrollBatches.values()) {
        if (!batch.obsolete) deadline = Math.min(deadline, batch.deadline);
    }
    if (deadline !== Infinity) applicationScrollTimer = setTimeout(applicationScrollAckTimedOut,
        Math.max(1, Math.ceil(deadline - performance.now())));
}
function applicationScrollAckTimedOut() {
    applicationScrollTimer = 0;
    if (!applicationScrollInFlight) return;
    applicationScrollTimeouts++;
    applicationScrollStalled = true;
    clearTimeout(applicationScrollSendTimer);
    applicationScrollSendTimer = 0;
    // Keep the receipts and direction. A real reply or a fresh direction can
    // recover even when the unsent debt is exactly zero.
    animateVisualOffset(0);
}
window.tty7ScrollAcknowledged = function(gen, id) {
    if (gen !== generation) return;
    const batch = applicationScrollBatches.get(id);
    if (!batch) return;
    applicationScrollBatches.delete(id);
    if (!batch.obsolete) {
        const now = performance.now();
        const latency = Math.max(0, now - batch.sentAt);
        applicationScrollAckEwma = applicationScrollAckEwma
            ? applicationScrollAckEwma * 0.75 + latency * 0.25 : latency;
        applicationScrollStalled = false;
        applicationScrollTimeouts = 0;
        // A valid reply proves link progress, not completion of other batches.
        // Give other overdue receipts time to arrive; their rows remain charged.
        for (const pending of applicationScrollBatches.values()) {
            if (!pending.obsolete && pending.deadline <= now) pending.deadline = now + applicationScrollTimeout();
        }
        refreshApplicationScroll();
        // Delivery does not confirm application paint. Only settle the lead;
        // screen revisions cannot refill the transport window.
        animateVisualOffset(applicationScrollTarget());
    }
    armApplicationScrollTimeout();
    scheduleApplicationScroll();
};
function scheduleApplicationScroll() {
    if (applicationScrollStalled || applicationScrollSendTimer) return;
    if (!applicationScrolls()) { cancelApplicationScroll(); return; }
    if (Math.abs(applicationScrollDebt) < 1 || applicationScrollCredit() < 1 ||
        applicationScrollBatches.size >= maxApplicationScrollOutstandingBatches) return;
    const interval = Math.abs(applicationScrollDebt) >= maxApplicationScrollBatch ||
        applicationScrollLastBatchRows >= maxApplicationScrollBatch
        ? bulkApplicationScrollInterval : minApplicationScrollInterval;
    const delay = applicationScrollSentAt ? interval - (performance.now() - applicationScrollSentAt) : 0;
    if (delay > 0) {
        applicationScrollSendTimer = setTimeout(() => {
            applicationScrollSendTimer = 0;
            scheduleApplicationScroll();
        }, Math.ceil(delay));
    } else sendApplicationScroll();
}
function sendApplicationScroll() {
    if (applicationScrollStalled || applicationScrollBatches.size >= maxApplicationScrollOutstandingBatches) return;
    if (!applicationScrolls()) { cancelApplicationScroll(); return; }
    const limit = Math.min(maxApplicationScrollBatch, applicationScrollCredit());
    const lines = Math.max(-limit, Math.min(limit, Math.trunc(applicationScrollDebt)));
    if (!lines) return;
    applicationScrollDebt -= lines;
    applicationScrollSentSign = Math.sign(lines);
    applicationScrollSentAt = performance.now();
    applicationScrollLastBatchRows = Math.abs(lines);
    const id = ++applicationScrollBatchId;
    applicationScrollBatches.set(id, {rows:Math.abs(lines), sentAt:applicationScrollSentAt,
        deadline:applicationScrollSentAt + applicationScrollTimeout(), obsolete:false});
    refreshApplicationScroll();
    const modes = applicationScreen.modes;
    let sequence;
    if (modes.mouse) {
        const col = Math.max(1, Math.min(terminal.cols, 1 + Math.floor((scrollPoint.x - scrollOrigin.left) / cellSize.width)));
        const row = Math.max(1, Math.min(terminal.rows, 1 + Math.floor((scrollPoint.y - scrollOrigin.top) / cellSize.height)));
        const button = lines < 0 ? 64 : 65;
        sequence = modes.sgr_mouse ? `\x1b[<${button};${col};${row}M` :
            `\x1b[M${String.fromCharCode(button + 32, Math.min(col,223) + 32, Math.min(row,223) + 32)}`;
    } else sequence = modes.app_cursor ? (lines < 0 ? '\x1bOA' : '\x1bOB') : (lines < 0 ? '\x1b[A' : '\x1b[B');
    armApplicationScrollTimeout();
    updateVisualOffset();
    if (window.Tty7) window.Tty7.scroll(generation, id, sequence.repeat(Math.abs(lines)));
    scheduleApplicationScroll();
}
function scrollLocally(amount) {
    if (historyOpening) {
        historyPendingScroll = Math.min(0, historyPendingScroll + amount);
        if (amount > 0 && historyPendingScroll === 0) { returnLive(); return false; }
        return true;
    }
    if (!history.hidden) {
        const remaining = moveScroll(history, amount);
        if (amount > 0 && isAtHistoryBottom() && !historyNewerBoundary) { returnLive(); return false; }
        historyScrollTop = history.scrollTop;
        updateHistoryStatus();
        if (amount < 0 && history.scrollTop < 160) requestHistory();
        following = false;
        return remaining !== amount;
    }
    // Full-screen programs own their transcript (Codex, vim, less, tmux).
    // Match the desktop's wheel route rather than opening shell history.
    if (applicationScrolls()) {
        if (amount * (applicationScrollDebt || applicationScrollSentSign) < 0) {
            cancelApplicationScroll();
        }
        applicationScrollDebt = Math.max(-maxApplicationScrollDebt,
            Math.min(maxApplicationScrollDebt,
                applicationScrollDebt + amount / applicationScrollRowHeight()));
        // Start immediately, then coalesce moves while the bounded row window
        // has room. Only matching RPC replies return transport credit.
        scheduleApplicationScroll();
        updateVisualOffset();
        return true;
    }
    const remaining = moveScroll(viewport, amount);
    // scrollTop rounds fractional touch deltas; a negative remainder alone
    // does not mean the reader moved upward or reached the top.
    if (amount < 0 && remaining < 0 && viewport.scrollTop === 0 && inputReady && showHistory()) return scrollLocally(remaining);
    following = isAtLive();
    return remaining !== amount;
}

// A drag scrolls locally or tells a full-screen program to scroll, and never
// focuses the IME. Only an intentional tap may focus it.
const touchSlop = 8;
const activePointers = new Set();
let touch;
let interrupted = false;
let frame = 0;
let coast = 0;
let pendingX = 0;
let pendingY = 0;
let suppressMouseUntil = 0;
function focusInput() {
    input.readOnly = false;
    terminal.focus();
    if (window.Tty7) window.Tty7.focusInput(generation);
}
function stopScrolling() {
    cancelAnimationFrame(frame);
    cancelAnimationFrame(coast);
    cancelApplicationScroll();
    touch = undefined;
    frame = coast = 0;
    pendingX = pendingY = 0;
}
function flushScroll() {
    cancelAnimationFrame(frame);
    frame = 0;
    const horizontal = pendingX;
    const vertical = pendingY;
    pendingX = pendingY = 0;
    if (horizontal) (history.hidden ? viewport : history).scrollLeft += horizontal;
    if (vertical) {
        const moved = scrollLocally(vertical);
        if (touch?.axis === 'y') touch.blocked = !moved;
    }
}
function trackTouch(point, time) {
    scrollPoint = {x:point.clientX, y:point.clientY};
    if (!touch.axis) {
        const dx = Math.abs(point.clientX - touch.startX);
        const dy = Math.abs(point.clientY - touch.startY);
        if (Math.max(dx, dy) < touchSlop) return;
        touch.axis = dy > dx ? 'y' : 'x';
    }
    if (touch.axis === 'x') pendingX += touch.x - point.clientX;
    else if (touch.y !== point.clientY) {
        pendingY += touch.y - point.clientY;
        touch.samples.push([point.clientY, time]);
        // Old movement must not dilute the velocity of a fresh flick.
        while (touch.samples.length > 2 && time - touch.samples[0][1] > 80) touch.samples.shift();
    }
    touch.x = point.clientX;
    touch.y = point.clientY;
}
function glide(samples, endedAt) {
    if (samples.length < 2) return;
    const remote = applicationScrolls();
    const [y0, t0] = samples[0];
    const [y1, t1] = samples[samples.length - 1];
    // Holding still before lifting stops the scroll instead of flinging it.
    if (endedAt - t1 > 80) return;
    let velocity = (y0 - y1) / Math.max(1, endedAt - t0);
    let at = performance.now();
    const startedAt = at;
    const step = now => {
        coast = 0;
        const elapsed = now - at;
        at = now;
        velocity *= Math.pow(remote ? 0.985 : 0.995, elapsed);
        if (Math.abs(velocity) < 0.02 || (remote && now - startedAt > 240)) return;
        if (!scrollLocally(velocity * Math.min(elapsed, 32))) return;
        coast = requestAnimationFrame(step);
    };
    coast = requestAnimationFrame(step);
}
// xterm replaces rendered rows while scrolling. A TouchEvent stays targeted at
// that detached row, so its later moves/end never reach this viewport. Capture
// PointerEvents on the stable viewport instead of tracking the rendered cells.
viewport.addEventListener('pointerdown', event => {
    if (event.target.closest('button')) return;
    if (event.pointerType === 'mouse') return;
    event.stopPropagation();
    stopScrolling();
    suppressMouseUntil = performance.now() + 700;
    activePointers.add(event.pointerId);
    if (activePointers.size !== 1 || interrupted) {
        viewport.setPointerCapture(event.pointerId);
        event.preventDefault();
        interrupted = true;
        touch = undefined;
        return;
    }
    blurInput();
    const rect = screen.getBoundingClientRect();
    scrollOrigin = {left:rect.left, top:rect.top};
    viewport.setPointerCapture(event.pointerId);
    event.preventDefault();
    touch = {
        id:event.pointerId, x:event.clientX, y:event.clientY,
        startX:event.clientX, startY:event.clientY, axis:null,
        samples:[[event.clientY, event.timeStamp]]
    };
}, {capture:true});
viewport.addEventListener('pointermove', event => {
    if (!activePointers.has(event.pointerId)) return;
    event.stopPropagation();
    if (!touch || event.pointerId !== touch.id) return;
    event.preventDefault();
    trackTouch(event, event.timeStamp);
    if (!frame && (pendingX || pendingY)) frame = requestAnimationFrame(flushScroll);
}, {capture:true});
viewport.addEventListener('pointerup', event => {
    if (!activePointers.delete(event.pointerId)) return;
    event.stopPropagation();
    suppressMouseUntil = performance.now() + 700;
    if (interrupted) {
        event.preventDefault();
        interrupted = activePointers.size !== 0;
        touch = undefined;
        stopScrolling();
        return;
    }
    if (!touch || event.pointerId !== touch.id) return;
    // Include a final movement even if the WebView coalesced the last move.
    trackTouch(event, event.timeStamp);
    event.preventDefault();
    flushScroll();
    if (touch.axis === 'y' && !touch.blocked) glide(touch.samples, event.timeStamp);
    else if (!touch.axis && history.hidden && !historyOpening && inputReady) focusInput();
    touch = undefined;
}, {capture:true});
const cancelPointer = event => {
    if (!activePointers.delete(event.pointerId)) return;
    event.stopPropagation();
    suppressMouseUntil = performance.now() + 700;
    interrupted = activePointers.size !== 0;
    touch = undefined;
    stopScrolling();
};
viewport.addEventListener('pointercancel', cancelPointer, {capture:true});
viewport.addEventListener('lostpointercapture', cancelPointer, {capture:true});
// xterm's own touch recognizer must not also scroll or produce PTY input.
for (const type of ['touchstart','touchmove','touchend','touchcancel']) {
    viewport.addEventListener(type, event => {
        if (event.target.closest('button')) return;
        event.stopPropagation();
        if (event.cancelable) event.preventDefault();
    }, {capture:true, passive:false});
}
viewport.addEventListener('wheel', event => {
    if (event.target.closest('button')) return;
    event.preventDefault();
    event.stopPropagation();
    stopScrolling();
    scrollPoint = {x:event.clientX, y:event.clientY};
    const rect = screen.getBoundingClientRect();
    scrollOrigin = {left:rect.left, top:rect.top};
    const factor = event.deltaMode === 1 ? cellSize.height :
        event.deltaMode === 2 ? viewport.clientHeight : 1;
    (history.hidden ? viewport : history).scrollLeft += event.deltaX * factor;
    scrollLocally(event.deltaY * factor);
}, {capture:true, passive:false});
for (const type of ['mousedown','mouseup','mousemove','contextmenu']) {
    terminal.element.addEventListener(type, event => {
        event.preventDefault();
        event.stopPropagation();
        if (type === 'mousedown' && inputReady && performance.now() >= suppressMouseUntil) focusInput();
    }, {capture:true});
}

window.addEventListener('resize', () => {
    sizeSurface();
    requestPhoneGrid();
    terminal.refresh(0, terminal.rows - 1);
    if (following) returnLive();
});
let writing = false;
let appliedRevision = null;
let appliedEpoch = null;
window.tty7Begin = function(gen) {
    generation = gen;
    inputReady = false;
    applicationScreen = null;
    sentGrid = '';
    applicationScrollBatches.clear();
    stopScrolling();
    touch = undefined;
    activePointers.clear();
    history.hidden = true;
    resetHistory();
    historyAgentKind = null;
    historyLive.hidden = true;
    blurInput();
    historyLoading = false;
    following = true;
    appliedRevision = appliedEpoch = null;
    // Drain at most the previous current-state write, never historical replay.
    terminalFontsReady.then(() => terminal.write('', () => {
        if (gen !== generation) return;
        terminal.reset();
        writing = false;
        sizeSurface();
        requestPhoneGrid();
        if (window.Tty7) window.Tty7.begun(gen);
    })).catch(error => fail(error.message));
    return true;
};
window.tty7Screen = function(gen, state, sequence) {
    if (gen !== generation) return false;
    try {
        if (writing) throw new Error('上一个终端画面尚未完成');
        if (state.base_revision !== null && (state.base_revision !== appliedRevision || state.epoch !== appliedEpoch)) throw new Error('终端画面基线不匹配');
        if (!Number.isInteger(state.cols) || !Number.isInteger(state.rows) || state.cols < 1 || state.cols > 1000 || state.rows < 1 || state.rows > 1000) throw new Error('终端尺寸无效');
        if (!Number.isSafeInteger(sequence) || sequence < 1) throw new Error('终端画面序号无效');
        if (state.base_revision === null) terminal.reset();
        if (terminal.cols !== state.cols || terminal.rows !== state.rows) terminal.resize(state.cols, state.rows);
        liveWidths.prepare(state.lines, state.cols);
        let paint = '\x1b[?7l\x1b[?25l';
        for (const line of state.lines) paint += `\x1b[${line.row + 1};1H\x1b[2K${line.ansi}\x1b[0m`;
        const cursor = state.cursor, modes = state.modes;
        paint += `\x1b[${cursor.row + 1};${cursor.col + 1}H`;
        paint += modes.app_cursor ? '\x1b[?1h' : '\x1b[?1l';
        paint += modes.app_keypad ? '\x1b=' : '\x1b>';
        paint += modes.bracketed_paste ? '\x1b[?2004h' : '\x1b[?2004l';
        paint += cursor.visible ? '\x1b[?25h' : '\x1b[?25l';
        if (terminal.options.cursorStyle !== cursor.shape) terminal.options.cursorStyle = cursor.shape;
        if (terminal.options.cursorBlink !== cursor.blink) terminal.options.cursorBlink = cursor.blink;
        writing = true;
        terminal.write(paint, () => {
            if (gen !== generation) return;
            try {
                // An empty history is final only for this screen revision.
                // New output may have pushed rows into retained history.
                if (state.revision !== appliedRevision && historyLoaded && !historyMore && !historyText.childElementCount) {
                    historyLoaded = false;
                    historyMore = true;
                }
                appliedRevision = state.revision; appliedEpoch = state.epoch;
                if (applicationScreen?.alternate !== state.alternate) stopScrolling();
                applicationScreen = {alternate:state.alternate, modes:state.modes};
                writing = false;
                if (window.Tty7) window.Tty7.rendered(gen, sequence);
            } catch (error) { writing = false; fail(error.message || '终端画面更新失败'); }
        });
        return true;
    } catch (error) {
        writing = false;
        fail(error.message || '终端画面读取失败');
        return false;
    }
};
terminalFontsReady.then(() => {
    terminal.options.fontFamily = terminalFontFamily;
    fontsReady = true;
    sizeSurface();
    // The bundled font can have the fallback's exact metrics; readiness must
    // still measure the initial phone grid when no geometry change was emitted.
    requestPhoneGrid();
    if (window.Tty7) window.Tty7.ready();
}).catch(error => fail(error.message));
