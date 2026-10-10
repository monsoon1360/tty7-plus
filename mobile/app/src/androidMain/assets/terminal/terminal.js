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
    cursorBlink:true, cursorInactiveStyle:'block', theme:{background:'#101014'}
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
    terminal.focus();
    requestPhoneGrid();
};
function sizeSurface() {
    // Grid changes come back in ordered PTY frames, never reflow ahead of output.
    terminal.element.style.height = screen.style.height;
    requestPhoneGrid();
}
function isAtLive() {
    return history.hidden &&
        terminal.buffer.active.viewportY === terminal.buffer.active.baseY &&
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
    if (following) viewport.scrollTop = viewport.scrollHeight;
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

let touch;
viewport.addEventListener('touchstart', event => {
    event.stopPropagation();
    if (event.touches.length !== 1) { touch = undefined; return; }
    const point = event.touches[0];
    touch = {x:point.clientX, y:point.clientY, startX:point.clientX, startY:point.clientY, moved:false};
}, {capture:true, passive:true});
viewport.addEventListener('touchmove', event => {
    event.stopPropagation();
    if (!touch || event.touches.length !== 1) return;
    event.preventDefault();
    const point = event.touches[0];
    touch.moved ||= Math.abs(point.clientX - touch.startX) + Math.abs(point.clientY - touch.startY) > 8;
    const horizontal = touch.x - point.clientX;
    const vertical = touch.y - point.clientY;
    (history.hidden ? viewport : history).scrollLeft += horizontal;
    scrollLocally(vertical);
    touch.x = point.clientX;
    touch.y = point.clientY;
}, {capture:true, passive:false});
viewport.addEventListener('touchend', event => {
    event.stopPropagation();
    // Leave toolbar buttons clickable, without xterm's synthesized mouse reports.
    if (touch && !touch.moved && history.hidden) {
        event.preventDefault();
        if (inputReady) terminal.focus();
    } else if (touch?.moved) event.preventDefault();
    touch = undefined;
}, {capture:true, passive:false});
viewport.addEventListener('touchcancel', event => {
    event.stopPropagation();
    touch = undefined;
}, {capture:true, passive:true});
viewport.addEventListener('wheel', event => {
    event.preventDefault();
    event.stopPropagation();
    const factor = event.deltaMode === 1 ? screen.clientHeight / terminal.rows :
        event.deltaMode === 2 ? viewport.clientHeight : 1;
    (history.hidden ? viewport : history).scrollLeft += event.deltaX * factor;
    scrollLocally(event.deltaY * factor);
}, {capture:true, passive:false});
for (const type of ['mousedown','mouseup','mousemove','contextmenu']) {
    terminal.element.addEventListener(type, event => {
        event.preventDefault();
        event.stopPropagation();
        if (type === 'mousedown' && inputReady) terminal.focus();
    }, {capture:true});
}

window.addEventListener('resize', () => {
    sizeSurface();
    terminal.refresh(0, terminal.rows - 1);
    if (following) viewport.scrollTop = viewport.scrollHeight;
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
                if (following) viewport.scrollTop = viewport.scrollHeight;
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
