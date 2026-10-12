'use strict';

function fail(message) {
    if (window.Tty7) window.Tty7.failed(message);
}
window.addEventListener('error', event => fail(event.message || '终端脚本执行失败'));
window.addEventListener('unhandledrejection', () => fail('终端渲染任务失败'));

const viewport = document.getElementById('viewport');
const history = document.getElementById('history');
const historyText = document.getElementById('history-text');
const terminal = new Terminal({
    cols:80, rows:24, fontSize:14, fontFamily:'monospace', scrollback:5000,
    convertEol:false, allowProposedApi:false, altClickMovesCursor:false,
    cursorBlink:true, cursorInactiveStyle:'block',
    theme:{background:'#101014', foreground:'#eeeeee', cursor:'#eeeeee', cursorAccent:'#101014'},
    // Desktop applications can emit dark ANSI/truecolour text for a light theme.
    // Keep their colours where readable, correcting contrast against each cell.
    minimumContrastRatio:4.5
});
terminal.open(document.getElementById('terminal'));
const screen = terminal.element.querySelector('.xterm-screen');
const scrollback = terminal.element.querySelector('.xterm-viewport');

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
    if (window.Tty7) window.Tty7.input(data);
});

let following = true;
let inputReady = false;
let sizeTimer;
let sentGrid = '';
function requestPhoneGrid() {
    clearTimeout(sizeTimer);
    sizeTimer = setTimeout(() => {
        if (!inputReady || !screen.clientWidth || !screen.clientHeight) return;
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
window.tty7Synced = function() {
    inputReady = true;
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

function showHistory() {
    // Alternate screens intentionally have no scrollback. Present the preserved
    // normal buffer read-only instead of injecting arrows or changing live modes.
    const normal = terminal.buffer.normal;
    const lines = [];
    for (let i = 0; i < normal.length; ++i) lines.push(normal.getLine(i).translateToString(true));
    if (!lines.some(line => line.trim())) return false;
    historyText.textContent = lines.join('\n');
    historyText.style.minWidth = screen.style.width;
    history.hidden = false;
    history.scrollTop = history.scrollHeight;
    history.scrollLeft = viewport.scrollLeft;
    terminal.blur();
    return true;
}

function moveScroll(element, amount) {
    const previous = element.scrollTop;
    element.scrollTop += amount;
    return amount - (element.scrollTop - previous);
}
function scrollLocally(amount) {
    if (!history.hidden) {
        moveScroll(history, amount);
        following = false;
        return;
    }
    if (amount < 0) {
        amount = moveScroll(viewport, amount);
        if (amount < 0) {
            if (terminal.buffer.active.type === 'alternate') {
                if (showHistory()) moveScroll(history, amount);
            } else moveScroll(scrollback, amount);
        }
    } else {
        amount = moveScroll(scrollback, amount);
        if (amount > 0) moveScroll(viewport, amount);
    }
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
    terminal.focus();
    if (window.Tty7) window.Tty7.focusInput();
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
let initialSnapshot = true;
let writing = false;
window.tty7Write = function(data, snapshot, cols, rows, sequence) {
    try {
        if (writing) throw new Error('上一个终端画面尚未完成');
        if (!Number.isInteger(cols) || !Number.isInteger(rows) || cols < 1 || cols > 1000 || rows < 1 || rows > 1000) {
            throw new Error('终端尺寸无效');
        }
        if (!Number.isSafeInteger(sequence) || sequence < 1) throw new Error('终端画面序号无效');
        const normalized = data.replace(/-/g, '+').replace(/_/g, '/');
        const raw = atob(normalized);
        const bytes = new Uint8Array(raw.length);
        for (let i = 0; i < raw.length; ++i) bytes[i] = raw.charCodeAt(i);
        // A replay consists of multiple snapshot chunks, including split UTF-8
        // and ANSI sequences. Reset once, never between replay segments.
        if (snapshot && initialSnapshot) { terminal.reset(); initialSnapshot = false; }
        if (terminal.cols !== cols || terminal.rows !== rows) terminal.resize(cols, rows);
        writing = true;
        terminal.write(bytes, () => {
            try {
                sizeSurface();
                if (terminal.buffer.active.type !== 'alternate') history.hidden = true;
                // Restoring the normal buffer or changing grid can leave xterm's
                // inner viewport in history even while the outer view follows.
                if (following) returnLive();
                writing = false;
                // Acknowledge parsed data even when offscreen or scrolled back;
                // animation-frame painting may be throttled by the WebView.
                if (window.Tty7) window.Tty7.rendered(sequence);
            } catch (error) {
                writing = false;
                fail(error.message || '终端画面更新失败');
            }
        });
        return true;
    } catch (error) {
        writing = false;
        fail(error.message || '终端画面读取失败');
        return false;
    }
};
sizeSurface();
if (window.Tty7) window.Tty7.ready();
