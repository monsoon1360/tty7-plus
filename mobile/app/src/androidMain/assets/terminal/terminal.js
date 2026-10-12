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
const historyScreen = document.getElementById('history-screen');
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
let nativeDark = null;
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
systemTheme.addEventListener('change', () => { terminal.options.theme = terminalTheme(); });
// Android WebView's media query can follow the Activity theme instead of the
// Compose system appearance. The native surface supplies the definitive value.
window.tty7Appearance = function(dark) {
    nativeDark = dark;
    document.documentElement.dataset.theme = dark ? 'dark' : 'light';
    terminal.options.theme = terminalTheme();
};
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
    if (inputReady && window.Tty7) window.Tty7.input(generation, data);
});

let following = true;
let inputReady = false;
let fontsReady = false;
let sizeTimer;
let sentGrid = '';
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
    terminal.element.style.height = screen.style.height;
    requestPhoneGrid();
}
function isAtLive() {
    return history.hidden &&
        // DOM scrollTop changes immediately; xterm's viewportY catches up later.
        scrollback.scrollTop >= scrollback.scrollHeight - scrollback.clientHeight - 1 &&
        viewport.scrollTop >= viewport.scrollHeight - viewport.clientHeight - 1;
}
function returnLive() {
    history.hidden = true;
    terminal.scrollToBottom();
    viewport.scrollTop = viewport.scrollHeight;
    following = true;
}
document.getElementById('return-live').addEventListener('click', returnLive);
terminal.onRender(() => {
    // WebView may measure fonts after the first write/attachment.
    sizeSurface();
    if (following) returnLive();
});

const historyStatus = document.getElementById('history-status');
const historyParser = new Terminal({cols:80, rows:1, scrollback:0, allowProposedApi:false});
const historyWidths = installCanonicalWidths(historyParser);
let historyCursor = null;
let historyMore = true;
let historyLoading = false;
let historyLoaded = false;
let historyBytes = 0;
// A separate read-only styled cache, at most 2,000 rows / 2 MiB UTF-8.
const maxHistoryRows = 2000;
const maxHistoryBytes = 2 * 1024 * 1024;
function requestHistory() {
    if (!inputReady || historyLoading || !historyMore) return;
    historyLoading = true;
    historyStatus.textContent = '正在加载历史…';
    if (window.Tty7) window.Tty7.history(generation, historyCursor || '');
}
function showHistory() {
    if (historyLoaded && !historyMore && !historyText.childElementCount) return false;
    following = false;
    // Retained history excludes the current screen. Preserve that screen while
    // loading and at the bottom of history, rather than displaying an empty layer.
    historyScreen.replaceChildren();
    for (let row = 0; row < terminal.rows; ++row) {
        const text = terminal.buffer.active.getLine(row).translateToString(false);
        historyScreen.appendChild(historyRow({cols:terminal.cols, ansi:'', text}, row, terminal));
    }
    history.hidden = false;
    history.scrollLeft = viewport.scrollLeft;
    history.scrollTop = history.scrollHeight;
    blurInput();
    if (!historyLoaded) { historyMore = true; requestHistory(); }
    return true;
}
history.addEventListener('scroll', () => {
    if (!history.hidden && history.scrollTop < 160) requestHistory();
});
function color(cell, foreground) {
    const value = foreground ? cell.getFgColor() : cell.getBgColor();
    if (foreground ? cell.isFgRGB() : cell.isBgRGB()) return '#' + value.toString(16).padStart(6, '0');
    if (!(foreground ? cell.isFgPalette() : cell.isBgPalette())) return foreground ? terminalTheme().foreground : terminalTheme().background;
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
        let fg = color(cell, true), bg = color(cell, false);
        if (cell.isInverse()) [fg, bg] = [bg, fg];
        const style = `color:${readable(fg,bg)};background:${bg};font-weight:${cell.isBold() ? 'bold' : 'normal'};font-style:${cell.isItalic() ? 'italic' : 'normal'};text-decoration:${cell.isUnderline() ? 'underline' : ''} ${cell.isStrikethrough() ? 'line-through' : ''}`;
        const chars = cell.isInvisible() ? ' '.repeat(cell.getWidth()) : cell.getChars() || ' ';
        const positioned = /[^\x20-\x7e]/.test(chars) || cell.getWidth() !== 1;
        if (positioned || style !== previous || !run) {
            run = document.createElement('span');
            run.style.cssText = style;
            if (positioned) {
                run.style.display = 'inline-block';
                run.style.width = `${cell.getWidth()}ch`;
                run.style.overflow = 'hidden';
                run.style.verticalAlign = 'top';
            }
            row.appendChild(run);
            previous = style;
        }
        run.appendChild(document.createTextNode(chars));
        if (positioned) run = null;
    }
    return row;
}
window.tty7History = async function(gen, page) {
    if (gen !== generation || !historyLoading) return;
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
        else history.scrollTop = history.scrollHeight;
        let evicted = false;
        while (historyText.childElementCount > maxHistoryRows || historyBytes > maxHistoryBytes) {
            const last = historyText.lastElementChild;
            historyBytes -= Number(last.dataset.bytes);
            last.remove();
            evicted = true;
        }
        historyCursor = page.cursor;
        historyMore = page.has_more;
        historyLoaded = true;
        historyLoading = false;
        historyStatus.textContent = page.truncated || evicted ? '已保留历史（有边界截断）' : historyMore ? '向上滚动加载更多' : '已到历史起点';
        if (!historyMore && !historyText.childElementCount) returnLive();
    } catch (error) { window.tty7HistoryError(gen, error.message || '历史加载失败'); }
};
window.tty7HistoryError = function(gen, message) {
    if (gen !== generation) return;
    historyLoading = false;
    historyStatus.textContent = message + '；返回实时后重试';
    historyMore = false;
    historyCursor = null;
    historyLoaded = false;
    historyText.replaceChildren();
    historyBytes = 0;
};

function moveScroll(element, amount) {
    const previous = element.scrollTop;
    element.scrollTop += amount;
    return amount - (element.scrollTop - previous);
}
function scrollLocally(amount) {
    if (!history.hidden) {
        moveScroll(history, amount);
        if (amount < 0 && history.scrollTop < 160) requestHistory();
        following = false;
        return;
    }
    if (amount < 0) {
        amount = moveScroll(viewport, amount);
        if (amount < 0 && inputReady && showHistory()) moveScroll(history, amount);
    } else moveScroll(viewport, amount);
    following = isAtLive();
}

// A drag stays local: even on an alternate screen it never becomes a key or
// mouse report. Only an intentional tap may focus the IME.
const touchSlop = 8;
const touchScrollSpeed = 1.8;
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
    if (vertical) scrollLocally(vertical * touchScrollSpeed);
}
function trackTouch(point, time) {
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
    const [y0, t0] = samples[0];
    const [y1, t1] = samples[samples.length - 1];
    // Holding still before lifting stops the scroll instead of flinging it.
    if (endedAt - t1 > 80) return;
    let velocity = (y0 - y1) / Math.max(1, endedAt - t0);
    let at = performance.now();
    const step = now => {
        coast = 0;
        const elapsed = now - at;
        at = now;
        velocity *= Math.pow(0.995, elapsed);
        if (Math.abs(velocity) < 0.02) return;
        scrollLocally(velocity * Math.min(elapsed, 32) * touchScrollSpeed);
        coast = requestAnimationFrame(step);
    };
    coast = requestAnimationFrame(step);
}
// xterm replaces rendered rows while scrolling. A TouchEvent stays targeted at
// that detached row, so its later moves/end never reach this viewport. Capture
// PointerEvents on the stable viewport instead of tracking the rendered cells.
viewport.addEventListener('pointerdown', event => {
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
    // The history toolbar keeps its native click, but cannot start a fling.
    if (event.target.closest('button')) { touch = undefined; return; }
    blurInput();
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
    if (touch.axis === 'y') glide(touch.samples, event.timeStamp);
    else if (!touch.axis && history.hidden && inputReady) focusInput();
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
        event.stopPropagation();
        if (!event.target.closest('button') && event.cancelable) event.preventDefault();
    }, {capture:true, passive:false});
}
viewport.addEventListener('wheel', event => {
    event.preventDefault();
    event.stopPropagation();
    stopScrolling();
    const factor = event.deltaMode === 1 ? screen.clientHeight / terminal.rows :
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
    terminal.refresh(0, terminal.rows - 1);
    if (following) returnLive();
});
let writing = false;
let appliedRevision = null;
let appliedEpoch = null;
window.tty7Begin = function(gen) {
    generation = gen;
    inputReady = false;
    sentGrid = '';
    stopScrolling();
    touch = undefined;
    activePointers.clear();
    history.hidden = true;
    historyText.replaceChildren();
    historyScreen.replaceChildren();
    blurInput();
    historyCursor = null; historyMore = true; historyLoading = false; historyLoaded = false; historyBytes = 0;
    following = true;
    appliedRevision = appliedEpoch = null;
    // Drain at most the previous current-state write, never historical replay.
    terminalFontsReady.then(() => terminal.write('', () => {
        if (gen !== generation) return;
        terminal.reset();
        writing = false;
        sizeSurface();
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
        terminal.options.cursorStyle = cursor.shape;
        terminal.options.cursorBlink = cursor.blink;
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
                sizeSurface();
                if (following) returnLive();
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
    if (window.Tty7) window.Tty7.ready();
}).catch(error => fail(error.message));
