const assert = require('node:assert/strict');
const test = require('node:test');
const fs = require('node:fs');
const os = require('node:os');
const path = require('node:path');
const {pathToFileURL} = require('node:url');
const {spawn} = require('node:child_process');

// Run the actual terminal assets in a disposable Chromium profile, without npm
// browser dependencies. Hosts without a browser can set TTY7_TEST_BROWSER.
const browser = [process.env.TTY7_TEST_BROWSER,
  '/Applications/Google Chrome.app/Contents/MacOS/Google Chrome',
  '/Applications/Microsoft Edge.app/Contents/MacOS/Microsoft Edge',
  '/usr/bin/chromium', '/usr/bin/chromium-browser', '/usr/bin/google-chrome',
].find(candidate => candidate && fs.existsSync(candidate));

test('terminal keeps one-screen contents and distinguishes taps from history drags',
  {skip: !browser && 'Chromium is unavailable; set TTY7_TEST_BROWSER', timeout:45000}, async t => {
  const profile = fs.mkdtempSync(path.join(os.tmpdir(), 'tty7-terminal-test-'));
  const url = pathToFileURL(path.resolve(__dirname, '../../androidMain/assets/terminal/index.html')).href;
  const browserProcess = spawn(browser, ['--headless=new', '--disable-gpu', '--no-first-run',
    '--no-default-browser-check', '--remote-debugging-port=0', `--user-data-dir=${profile}`, url],
    {stdio:['ignore','ignore','pipe']});
  t.after(async () => {
    if (browserProcess.exitCode === null) {
      browserProcess.kill();
      await new Promise(resolve => browserProcess.once('exit', resolve));
    }
    fs.rmSync(profile, {recursive:true, force:true});
  });
  const endpoint = await new Promise((resolve, reject) => {
    let log = '';
    const timer = setTimeout(() => reject(new Error('Browser startup timed out')), 10000);
    browserProcess.once('error', error => { clearTimeout(timer); reject(error); });
    browserProcess.stderr.on('data', data => {
      log += data;
      const match = log.match(/DevTools listening on ws:\/\/([^/]+)/);
      if (match) { clearTimeout(timer); resolve(`http://${match[1]}`); }
    });
  });
  let page;
  for (let attempt = 0; attempt < 100 && !page; ++attempt) {
    const pages = await (await fetch(`${endpoint}/json/list`)).json();
    page = pages.find(p => p.url === url);
    if (!page) await new Promise(resolve => setTimeout(resolve, 20));
  }
  assert.ok(page, 'The terminal test page opened');
  const ws = new WebSocket(page.webSocketDebuggerUrl);
  await new Promise(resolve => ws.addEventListener('open', resolve, {once:true}));
  t.after(() => ws.close());
  let id = 0;
  const pending = new Map();
  ws.addEventListener('message', ({data}) => {
    const message = JSON.parse(data);
    const request = pending.get(message.id);
    if (!request) return;
    clearTimeout(request.timer);
    pending.delete(message.id);
    if (message.error) request.reject(message.error);
    else request.resolve(message.result);
  });
  const call = (method, params = {}) => new Promise((resolve,reject) => {
    const key = ++id;
    const timer = setTimeout(() => { pending.delete(key); reject(new Error(`CDP timed out: ${method}`)); }, 5000);
    pending.set(key,{resolve,reject,timer}); ws.send(JSON.stringify({id:key, method, params}));
  });
  const evaluate = async expression => {
    const result = await call('Runtime.evaluate', {expression, returnByValue:true, awaitPromise:true});
    if (result.exceptionDetails) throw new Error(JSON.stringify(result.exceptionDetails));
    return result.result.value;
  };
  const wait = async expression => {
    for (let n = 0; n < 100; n++) { if (await evaluate(expression)) return; await new Promise(r => setTimeout(r,20)); }
    throw new Error('Timed out: '+expression);
  };
  await call('Emulation.setDeviceMetricsOverride', {width:390,height:700,deviceScaleFactor:1,mobile:true});
  await call('Emulation.setTouchEmulationEnabled', {enabled:true});
  await call('Page.bringToFront');
  await wait('typeof window.tty7Begin === "function"');
  await evaluate(`window.calls={focus:0,history:0,input:[],scroll:[],grids:[],rendered:0,begun:false}; window.Tty7={ready(){},resize(cols,rows){calls.grids.push([cols,rows])},begun(){calls.begun=true},rendered(){calls.rendered++},failed(g,m){throw new Error(m)},history(){calls.history++},focusInput(){calls.focus++},input(g,t){calls.input.push(t)},scroll(g,b,t){calls.scroll.push({generation:g,batch:b});calls.input.push(t)}}; window.tty7Begin(1)`);
  await wait('calls.begun');
  await wait('calls.grids.length > 0');
  assert.equal(await evaluate('inputReady'), false, 'Initial phone grid is measured before subscription sync');
  assert.ok(await evaluate('calls.grids.every(([cols,rows]) => cols >= 20 && cols <= 1000 && rows >= 5 && rows <= 1000)'));
  assert.match(await evaluate('terminal.options.fontFamily'), /JetBrainsMono Nerd Font Mono/);
  assert.deepEqual(await evaluate(`Array.from(document.fonts).map(font => font.status)`), Array(5).fill('loaded'));
  // Check real canvas glyphs, not only CSS font declarations. Compare prompt
  // icons and agent symbols to the missing-glyph box with the same font stack.
  const glyphs = ['\ue0b0', '\ue0a0', '\uf07b', '\udb80\udc01', '⏺', '⎿', '✔', '⚠', '⠋', '✅'];
  const missing = await evaluate(`(() => {
    const canvas = document.createElement('canvas'); canvas.width = 96; canvas.height = 64;
    const ctx = canvas.getContext('2d'); ctx.font = '32px ' + terminal.options.fontFamily;
    const pixels = glyph => {ctx.clearRect(0,0,96,64);ctx.fillText(glyph,4,44);return Array.from(ctx.getImageData(0,0,96,64).data).join(',')};
    const tofu = pixels(String.fromCodePoint(0x10ffff));
    return ${JSON.stringify(glyphs)}.filter(glyph => pixels(glyph) === tofu);
  })()`);
  assert.deepEqual(missing, [], 'Bundled fonts render Nerd Font icons and agent symbols without tofu');
  assert.equal(await evaluate('getComputedStyle(historyText).fontFamily'), await evaluate('terminal.options.fontFamily'));
  await call('Emulation.setEmulatedMedia', {features:[{name:'prefers-color-scheme',value:'dark'}]});
  await wait('terminal.options.theme.background === "#1e1e20"');
  assert.equal(await evaluate('getComputedStyle(document.body).backgroundColor'), 'rgb(30, 30, 32)');
  await call('Emulation.setEmulatedMedia', {features:[{name:'prefers-color-scheme',value:'light'}]});
  await wait('terminal.options.theme.background === "#fcfcfb"');
  assert.equal(await evaluate('getComputedStyle(document.body).backgroundColor'), 'rgb(252, 252, 251)');
  await evaluate('window.tty7Appearance(true)');
  assert.equal(await evaluate('terminal.options.theme.background'), '#1e1e20');
  assert.equal(await evaluate('getComputedStyle(document.body).backgroundColor'), 'rgb(30, 30, 32)');
  await evaluate('window.tty7Appearance(false)');
  assert.equal(await evaluate('terminal.options.theme.background'), '#fcfcfb');
  assert.equal(await evaluate('terminal.options.theme.green'), '#1a7f37');
  t.diagnostic('PASS: offline fonts render prompt/agent glyphs; terminal and history share fonts and system appearance');
  const paint = text => Array.from(text.padEnd(40)).map((c,i)=>`\x1b[${i+1}G${c}`).join('');
  const state = {cols:40,rows:24,epoch:'1',revision:'1',base_revision:null,cursor:{row:1,col:0,shape:'block',visible:true,blink:false},modes:{app_cursor:false,app_keypad:false,bracketed_paste:false},lines:[{row:0,ansi:paint('Current screen')},{row:1,ansi:paint('prompt >')}]};
  await evaluate(`window.tty7Screen(1,${JSON.stringify(state)},1)`);
  await wait('calls.rendered === 1');
  await evaluate('window.tty7Synced(1); showHistory()');
  assert.equal(await evaluate('history.hidden'), true, 'Loading history never replaces live output with a placeholder');
  await evaluate(`window.tty7History(1,{lines:[],cursor:null,has_more:false,truncated:false})`);
  assert.equal(await evaluate('history.hidden'), true);
  const historyRequests = await evaluate('calls.history');
  await evaluate('scrollLocally(-100); scrollLocally(100); scrollLocally(-100)');
  assert.equal(await evaluate('history.hidden'), true);
  assert.equal(await evaluate('calls.history'), historyRequests);
  t.diagnostic('PASS: one-screen scrolling keeps live contents and stops empty-history retries');
  const touch = async (type,x,y) => call('Input.dispatchTouchEvent',{type,touchPoints:type==='touchEnd'?[]:[{x,y,id:1}]});
  await touch('touchStart',100,150); await touch('touchEnd',100,150);
  assert.equal(await evaluate('calls.focus'),1);
  assert.equal(await evaluate('input.readOnly'),false);
  assert.equal(await evaluate('document.activeElement === input'),true);
  await evaluate('window.tty7Appearance(true)');
  await call('Input.insertText', {text:'test input'});
  assert.deepEqual(await evaluate('calls.input'), ['test input']);
  assert.equal(await evaluate('terminal.options.theme.cursorAccent'), '#1e1e20');
  await call('Emulation.setEmulatedMedia', {features:[{name:'prefers-color-scheme',value:'light'}]});
  assert.equal(await evaluate('terminal.options.theme.background'), '#1e1e20', 'Manual black theme overrides the light system');
  assert.equal(await evaluate('getComputedStyle(document.documentElement).colorScheme'), 'dark');
  await evaluate('window.tty7Appearance(false)');
  await evaluate(`(async () => {
    calls.input = []; input.value = '';
    input.dispatchEvent(new KeyboardEvent('keydown', {key:'Unidentified',keyCode:229,bubbles:true}));
    const deletion = new InputEvent('beforeinput', {inputType:'deleteContentBackward',bubbles:true,cancelable:true});
    if (input.dispatchEvent(deletion)) {
      input.dispatchEvent(new InputEvent('input', {inputType:'deleteContentBackward',bubbles:true}));
    }
    await new Promise(resolve => setTimeout(resolve, 10));
  })()`);
  assert.deepEqual(await evaluate('calls.input'), ['\x7f'], 'Android 229 deletion reaches the remote editor even with an empty local buffer');
  await evaluate(`input.dispatchEvent(new KeyboardEvent('keyup', {key:'Unidentified',keyCode:229,bubbles:true}));
    input.dispatchEvent(new InputEvent('beforeinput', {inputType:'deleteContentBackward',bubbles:true,cancelable:true}))`);
  assert.deepEqual(await evaluate('calls.input'), ['\x7f','\x7f'], 'Repeated IME-only deletions are not lost');
  await evaluate(`calls.input = [];
    input.dispatchEvent(new KeyboardEvent('keydown', {key:'Backspace',keyCode:8,which:8,bubbles:true,cancelable:true}));
    input.dispatchEvent(new InputEvent('beforeinput', {inputType:'deleteContentBackward',bubbles:true,cancelable:true}));
    input.dispatchEvent(new KeyboardEvent('keyup', {key:'Backspace',keyCode:8,bubbles:true}))`);
  assert.deepEqual(await evaluate('calls.input'), ['\x7f'], 'Backspace plus beforeinput deletes exactly once');
  await evaluate('calls.input = []; input.value = ""');
  await call('Input.imeSetComposition', {text:'ni',selectionStart:2,selectionEnd:2});
  await call('Input.imeSetComposition', {text:'n',selectionStart:1,selectionEnd:1});
  assert.deepEqual(await evaluate('calls.input'), [], 'Editing Chinese candidates does not delete remote committed text');
  await call('Input.insertText', {text:'你'});
  await wait('calls.input.length > 0');
  assert.deepEqual(await evaluate('calls.input'), ['你'], 'The final Chinese candidate is committed exactly once');
  await evaluate(`calls.input = []; input.value = '';
    input.dispatchEvent(new CompositionEvent('compositionstart', {bubbles:true}));
    input.dispatchEvent(new InputEvent('beforeinput', {inputType:'deleteContentBackward',isComposing:true,bubbles:true,cancelable:true}))`);
  assert.deepEqual(await evaluate('calls.input'), [], 'Deleting an empty composition never erases remote committed text');
  await evaluate(`input.dispatchEvent(new CompositionEvent('compositionend', {data:'',bubbles:true}))`);
  t.diagnostic('PASS: IME-only deletion, Backspace deduplication and Chinese candidate editing preserve remote input');
  await evaluate(`(async () => {
    calls.input = []; input.value = '之前';
    for (const text of ['a', 'b']) {
      input.dispatchEvent(new KeyboardEvent('keydown', {key:'Unidentified',keyCode:229,bubbles:true}));
      input.value += text;
      input.dispatchEvent(new InputEvent('input', {inputType:'insertText',data:text,composed:true,bubbles:true}));
    }
    await new Promise(resolve => setTimeout(resolve, 10));
    input.dispatchEvent(new KeyboardEvent('keyup', {key:'Unidentified',keyCode:229,bubbles:true}));
  })()`);
  assert.equal(await evaluate('calls.input.join("")'), 'ab', 'Batched Android 229 edits send each new character once without replaying earlier text');
  for (const [before, after, expected] of [
    ['之前abc', '之前abd', '\x7fd'],
    ['之前😀', '之前好', '\x7f好'],
    ['之前ab', '之前a', '\x7f'],
    ['之前', '之前哈', '哈'],
  ]) {
    await evaluate(`(async () => {
      calls.input = []; input.value = ${JSON.stringify(before)};
      input.dispatchEvent(new KeyboardEvent('keydown', {key:'Unidentified',keyCode:229,bubbles:true}));
      input.value = ${JSON.stringify(after)};
      input.dispatchEvent(new InputEvent('input', {inputType:'insertText',data:${JSON.stringify(after)},bubbles:true}));
      await new Promise(resolve => setTimeout(resolve, 10));
      input.dispatchEvent(new KeyboardEvent('keyup', {key:'Unidentified',keyCode:229,bubbles:true}));
    })()`);
    assert.equal(await evaluate('calls.input.join("")'), expected, 'IME replacements/deletions send only the changed suffix, even without a composed flag');
  }
  await evaluate(`(async () => {
    calls.input = []; input.value = '之前';
    for (const text of ['哈', '哈']) {
      input.dispatchEvent(new KeyboardEvent('keydown', {key:'Unidentified',keyCode:229,bubbles:true}));
      input.value += text;
    }
    await new Promise(resolve => setTimeout(resolve, 10));
    input.dispatchEvent(new KeyboardEvent('keyup', {key:'Unidentified',keyCode:229,bubbles:true}));
  })()`);
  assert.equal(await evaluate('calls.input.join("")'), '哈哈', 'The timer fallback preserves intentional repeated characters without input events');
  await evaluate(`(async () => {
    calls.input = []; input.value = '';
    input.dispatchEvent(new CompositionEvent('compositionstart', {bubbles:true}));
    input.value = '你';
    input.dispatchEvent(new CompositionEvent('compositionupdate', {data:'你',bubbles:true}));
    input.dispatchEvent(new CompositionEvent('compositionend', {data:'你',bubbles:true}));
    input.dispatchEvent(new KeyboardEvent('keydown', {key:'Unidentified',keyCode:229,bubbles:true}));
    input.value += '!';
    input.dispatchEvent(new InputEvent('input', {inputType:'insertText',data:'!',bubbles:true}));
    await new Promise(resolve => setTimeout(resolve, 10));
    input.dispatchEvent(new KeyboardEvent('keyup', {key:'Unidentified',keyCode:229,bubbles:true}));
  })()`);
  assert.equal(await evaluate('calls.input.join("")'), '你!', 'Composition confirmation and following punctuation in one turn are committed once');
  await evaluate(`(async () => {
    calls.input = []; input.value = '之前';
    const edit = text => {
      input.dispatchEvent(new KeyboardEvent('keydown', {key:'Unidentified',keyCode:229,bubbles:true}));
      input.value += text;
      input.dispatchEvent(new InputEvent('input', {inputType:'insertText',data:text,composed:true,bubbles:true}));
    };
    edit('a');
    const clipboard = new DataTransfer();
    clipboard.setData('text/plain', '粘贴\\n两行');
    input.dispatchEvent(new ClipboardEvent('paste', {clipboardData:clipboard,bubbles:true,cancelable:true}));
    edit('b');
    await new Promise(resolve => setTimeout(resolve, 10));
    input.dispatchEvent(new KeyboardEvent('keyup', {key:'Unidentified',keyCode:229,bubbles:true}));
  })()`);
  assert.equal(await evaluate('calls.input.join("")'), 'a粘贴\r两行b', 'Paste owns its textarea reset without replaying or deleting earlier IME input');
  t.diagnostic('PASS: Android IME batches, autocorrection, Unicode deletion and composition handoff never replay committed text');
  await evaluate('calls.input = []');
  for (const [from,to] of [[200,400],[400,200]]) {
    await touch('touchStart',100,from);
    for (let y=1;y<=10;y++) await touch('touchMove',100,from+(to-from)*y/10);
    await touch('touchEnd',100,to);
    assert.equal(await evaluate('calls.focus'),1);
    assert.equal(await evaluate('input.readOnly'),true);
  }
  assert.deepEqual(await evaluate('calls.input'),[]);
  t.diagnostic('PASS: tap focuses input; upward/downward drags do not refocus or send PTY input');
  await evaluate('stopScrolling()');
  state.revision = '2';
  await evaluate(`window.tty7Screen(1,${JSON.stringify(state)},2)`);
  await wait('calls.rendered === 2');
  await evaluate('showHistory()');
  assert.equal(await evaluate('calls.history'), historyRequests + 1);
  const line = {cols:40,ansi:paint('Older retained line'),text:'Older retained line'};
  const capturedScreen = {cols:40,ansi:paint('Current screen'),text:'Current screen'};
  await evaluate(`window.tty7History(1,{lines:${JSON.stringify([line, capturedScreen])},cursor:'older',has_more:true,truncated:false})`);
  assert.match(await evaluate('historyText.textContent'), /Older retained line/);
  assert.match(await evaluate('historyText.textContent'), /Current screen/);
  assert.equal(await evaluate('history.hidden'),false);
  await evaluate('requestHistory(); window.tty7HistoryError(1,"模拟加载失败")');
  assert.match(await evaluate('historyText.textContent'), /Current screen/, 'An older-page failure keeps the coherent window visible');
  await evaluate('returnLive()');
  assert.equal(await evaluate('history.hidden'),true);
  t.diagnostic('PASS: retained history and error recovery keep the current screen available');

  // Use a shared desktop grid taller than the phone, so leaving history must
  // also restore the outer live viewport to its actual lower boundary.
  state.revision = '3';
  state.rows = 70;
  await evaluate(`window.tty7Screen(1,${JSON.stringify(state)},3)`);
  await wait('calls.rendered === 3');
  await evaluate('viewport.scrollTop = 20; scrollLocally(0.6)');
  assert.equal(await evaluate('history.hidden'),true,
    'Rounding a downward subpixel movement cannot open history');
  await evaluate('viewport.scrollTop = 20; scrollLocally(-0.3)');
  assert.equal(await evaluate('history.hidden'),true,
    'Unconsumed subpixel rounding before the live viewport top is not a history gesture');
  await evaluate('returnLive()');
  await evaluate('showHistory()');
  const lines = Array.from({length:250}, (_,index) => ({
    cols:40,ansi:paint(`History row ${index}`),text:`History row ${index}`,
  }));
  await evaluate(`window.tty7History(1,{lines:${JSON.stringify(lines)},cursor:'oldest',has_more:true,truncated:false})`);
  const retainedText = await evaluate('historyText.textContent');
  const retainedRequests = await evaluate('calls.history');
  for (const dark of [true, false]) {
    await evaluate(`window.tty7Appearance(${dark})`);
    assert.equal(await evaluate('history.hidden'), false, 'Theme changes keep history open');
    assert.equal(await evaluate('historyText.textContent'), retainedText);
    assert.equal(await evaluate('calls.history'), retainedRequests, 'Theme changes reuse cached history');
    assert.equal(await evaluate(`(() => {
      const hex = rgb => '#' + rgb.match(/\\d+/g).slice(0,3).map(n => Number(n).toString(16).padStart(2,'0')).join('');
      return [...historyText.querySelectorAll('span')].every(span => {
        const style = getComputedStyle(span);
        const fg = luminance(hex(style.color)), bg = luminance(hex(style.backgroundColor));
        return (Math.max(fg,bg)+0.05)/(Math.min(fg,bg)+0.05) >= 4.5;
      });
    })()`), true, 'Both themes maintain readable history text');
    assert.equal(await evaluate('getComputedStyle(historyText.firstElementChild.firstElementChild).backgroundColor'),
      dark ? 'rgb(30, 30, 32)' : 'rgb(252, 252, 251)', 'Cached history uses the selected background');
  }
  t.diagnostic('PASS: black/white switching recolours cached history and preserves contents without fetching again');
  await evaluate('history.scrollTop = 300; historyScrollTop = history.scrollTop; requestHistory()');
  await evaluate('window.tty7History(1,{lines:[],cursor:null,has_more:false,truncated:false})');
  assert.equal(await evaluate('history.hidden'),false, 'Loading history does not leave the read-only view');
  await evaluate('history.scrollTop = 0');
  await wait('history.scrollTop === 0');
  await evaluate('scrollLocally(300)');

  // A real downward gesture reaches live mode without opening the keyboard;
  // the following tap may immediately focus and send input.
  await evaluate('history.scrollTop = history.scrollHeight - history.clientHeight - 100; historyScrollTop = history.scrollTop');
  const focusBeforeReturn = await evaluate('calls.focus');
  await touch('touchStart',100,450);
  await touch('touchMove',100,250);
  await touch('touchEnd',100,250);
  await wait('history.hidden && following && isAtLive()');
  assert.equal(await evaluate('calls.focus'),focusBeforeReturn);
  await touch('touchStart',100,250); await touch('touchEnd',100,250);
  assert.equal(await evaluate('calls.focus'),focusBeforeReturn + 1);
  assert.equal(await evaluate('input.readOnly'),false);
  await call('Input.insertText', {text:'after history'});
  assert.deepEqual(await evaluate('calls.input'),['after history']);

  // Loading history must preserve finger travel without replacing live output
  // before the coherent page is ready.
  const delayedRows = Array.from({length:100}, (_,index) => ({
    cols:40, ansi:paint(`detail ${index}`), text:`detail ${index}`,
  }));
  await evaluate('returnLive(); resetHistory(); showHistory(); scrollLocally(-90)');
  assert.equal(await evaluate('history.hidden'),true, 'An in-flight history request leaves live output visible');
  await evaluate('scrollLocally(90)');
  await evaluate(`window.tty7History(1,{lines:${JSON.stringify(delayedRows)},cursor:null,has_more:false,truncated:false})`);
  assert.equal(await evaluate('history.hidden && following'),true,
    'Returning to bottom while loading prevents a late page from flashing over live output');
  await evaluate('returnLive(); resetHistory(); showHistory(); scrollLocally(-90); scrollLocally(-45); scrollLocally(10)');
  await evaluate(`window.tty7History(1,{lines:${JSON.stringify(delayedRows)},cursor:null,has_more:false,truncated:false})`);
  assert.equal(await evaluate('history.scrollHeight - history.clientHeight - history.scrollTop'),125,
    'The first history page preserves all pending finger travel in both directions');
  await evaluate('history.scrollTop = history.scrollHeight; historyScrollTop = history.scrollTop; updateHistoryStatus()');
  const bottomBarHeight = await evaluate('document.getElementById("history-toolbar").getBoundingClientRect().height');
  await evaluate('scrollLocally(-1)');
  assert.equal(await evaluate('document.getElementById("history-toolbar").getBoundingClientRect().height'),bottomBarHeight,
    'Revealing return-to-bottom cannot move the history viewport');
  assert.equal(await evaluate('history.scrollHeight - history.clientHeight - history.scrollTop'),1);
  await evaluate('scrollLocally(1)');
  await wait('history.hidden && following');
  await evaluate('scrollLocally(30); scrollLocally(30)');
  assert.equal(await evaluate('history.hidden'),true, 'Continued scrolling at live bottom never reopens history');

  await evaluate('showHistory(); scrollLocally(-300)');
  const detailTop = await evaluate('history.scrollTop');
  await touch('touchStart',100,300);
  await touch('touchMove',100,270);
  await wait(`Math.abs(history.scrollTop - ${detailTop} - 30) < 1`);
  await new Promise(resolve => setTimeout(resolve, 100));
  await touch('touchEnd',100,270);
  assert.ok(Math.abs(await evaluate('history.scrollTop') - detailTop - 30) < 1,
    'A slow drag positions table details at 1:1 finger travel, without a release jump');
  await evaluate('returnLive()');
  t.diagnostic('PASS: delayed first-page travel, stable bottom boundary, and precise 1:1 detail positioning');
  // Native scroll events (for example a scrollbar or accessibility scroll)
  // follow the same boundary rule, while opening cached history stays open.
  await evaluate('showHistory()');
  await new Promise(resolve => setTimeout(resolve, 50));
  assert.equal(await evaluate('history.hidden'),false);
  await evaluate('scrollLocally(-300)');
  await evaluate('history.scrollTop = history.scrollHeight');
  await wait('history.hidden && following && isAtLive()');
  t.diagnostic('PASS: dragging or scrolling to the bottom restores live mode and tap input');

  await evaluate('window.tty7Begin(2)');
  assert.equal(await evaluate('input.readOnly'),true);
  t.diagnostic('PASS: switching terminal clears history snapshot and input focus');
  await wait('!writing && generation === 2');
  const fullScreen = {...state, revision:'4', base_revision:null, epoch:'2', alternate:true,
    modes:{app_cursor:false,app_keypad:false,bracketed_paste:true,mouse:true,sgr_mouse:true,alternate_scroll:true}};
  await evaluate(`window.tty7Screen(2,${JSON.stringify(fullScreen)},4)`);
  await wait('calls.rendered === 4');
  await evaluate('window.tty7Synced(2); calls.input = []; stopScrolling()');
  const requestsBeforeAppScroll = await evaluate('calls.history');
  const focusBeforeAppScroll = await evaluate('calls.focus');
  await touch('touchStart',100,400);
  await touch('touchMove',100,550);
  await touch('touchEnd',100,550);
  await wait('calls.input.length > 0');
  await evaluate('stopScrolling()');
  assert.equal(await evaluate('history.hidden'),true, 'Codex owns full-screen transcript scrolling');
  assert.equal(await evaluate('calls.history'),requestsBeforeAppScroll, 'Full-screen drag does not fetch unrelated shell history');
  assert.equal(await evaluate('calls.focus'),focusBeforeAppScroll, 'Scrolling never opens the keyboard');
  assert.match(await evaluate('calls.input.join("")'), /\x1b\[<64;\d+;\d+M/, 'Upward scroll reaches the application as SGR wheel-up');
  await evaluate('calls.input = []; scrollLocally(80)');
  await wait('calls.input.length > 0');
  assert.match(await evaluate('calls.input.join("")'), /\x1b\[<65;\d+;\d+M/);
  fullScreen.revision = '5'; fullScreen.modes.mouse = false; fullScreen.modes.app_cursor = true;
  await evaluate(`window.tty7Screen(2,${JSON.stringify(fullScreen)},5)`);
  await wait('calls.rendered === 5');
  await evaluate('calls.input = []; scrollLocally(-80)');
  await wait('calls.input.length > 0');
  assert.match(await evaluate('calls.input.join("")'), /\x1bOA/, 'Programs without mouse reporting use application cursor keys');
  fullScreen.revision = '6'; fullScreen.alternate = false;
  await evaluate(`window.tty7Screen(2,${JSON.stringify(fullScreen)},6)`);
  await wait('calls.rendered === 6');
  await evaluate('calls.input = []; scrollLocally(-10000)');
  assert.equal(await evaluate('historyLoading'),true, 'Returning to the main screen restores paged terminal history');
  assert.deepEqual(await evaluate('calls.input'),[]);
  t.diagnostic('PASS: full-screen gestures route to application mouse/arrows; main-screen history stays local');

  // Fresh generations keep cached history, touch state and delayed wheel
  // batches from leaking between these regressions.
  let appGeneration = 2;
  let appSequence = 6;
  const startApp = async ({synced = true} = {}) => {
    await evaluate(`calls.begun = false; window.tty7Begin(${++appGeneration})`);
    await wait('calls.begun');
    const appState = {cols:40, rows:24, epoch:String(appGeneration), revision:String(++appSequence), base_revision:null,
      cursor:{row:1,col:0,shape:'block',visible:true,blink:false}, alternate:true,
      modes:{app_cursor:false,app_keypad:false,bracketed_paste:true,mouse:true,sgr_mouse:true,alternate_scroll:true},
      lines:[{row:0,ansi:paint('Application transcript')}]};
    const rendered = await evaluate('calls.rendered');
    await evaluate(`window.tty7Screen(${appGeneration},${JSON.stringify(appState)},${appSequence})`);
    await wait(`calls.rendered === ${rendered + 1}`);
    await evaluate(`calls.input = []; applicationScrollAckEwma = 0; ${synced ? `window.tty7Synced(${appGeneration})` : ''}`);
  };
  const wheelCount = `(calls.input.join('').match(/\\x1b\\[<6[45];\\d+;\\d+M/g) || []).length`;
  const wheels = inputs => inputs.join('').match(/\x1b\[<6[45];\d+;\d+M/g) || [];
  const settle = async (milliseconds = 800) => {
    await evaluate(`new Promise(resolve => setTimeout(resolve,${milliseconds}))`);
  };
  const startFling = async () => {
    await touch('touchStart',100,200);
    await touch('touchMove',100,280);
    await touch('touchMove',100,380);
    await touch('touchEnd',100,460);
    await wait(`${wheelCount} > 0`);
  };
  // The native bridge reports completion of this batch's input RPC. Screen
  // revisions carry no input ID and must never act as a delivery reply.
  const ackApplicationScroll = async () => {
    // Drain current-intent receipts. Obsolete requests still consume transport
    // credit, but acknowledging them is explicit in the cancellation tests.
    for (let n = 0; n < 32; n++) {
      const id = await evaluate('[...applicationScrollBatches].find(([,b]) => !b.obsolete)?.[0] || 0');
      if (id) await evaluate(`window.tty7ScrollAcknowledged(${appGeneration},${id})`);
      await settle(40);
      if (await evaluate('!applicationScrollInFlight && !applicationScrollSendTimer')) break;
    }
    await wait('!applicationScrollInFlight && !applicationScrollSendTimer && !visualSettle');
  };
  const applyApplicationScreen = async ({lines = [], full = false} = {}) => {
    const revision = await evaluate('appliedRevision');
    const epoch = await evaluate('appliedEpoch');
    const cols = await evaluate('terminal.cols');
    const rows = await evaluate('terminal.rows');
    const rendered = await evaluate('calls.rendered');
    const next = {cols, rows, epoch, revision:String(++appSequence), base_revision:full ? null : revision,
      cursor:{row:Math.max(0, rows - 2), col:0, shape:'block', visible:true, blink:false}, alternate:true,
      modes:{app_cursor:false,app_keypad:false,bracketed_paste:true,mouse:true,sgr_mouse:true,alternate_scroll:true},
      lines};
    await evaluate(`window.tty7Screen(${appGeneration},${JSON.stringify(next)},${appSequence})`);
    await wait(`calls.rendered === ${rendered + 1}`);
  };

  await startApp();
  await evaluate('scrollLocally(-65.25 * screen.clientHeight / terminal.rows)');
  for (let batch = 0; batch < 6 && wheels(await evaluate('calls.input')).length < 65; batch++) {
    await ackApplicationScroll();
    await settle(20);
  }
  await wait(`${wheelCount} >= 65`);
  await settle(150);
  const largeScroll = await evaluate('calls.input');
  assert.equal(wheels(largeScroll).length, 65, 'A large gesture preserves every whole line of scroll debt');
  assert.ok(largeScroll.every(packet => {
    const events = wheels([packet]);
    return events.length > 0 && events.length <= 20 && events.every(event => event.startsWith('\x1b[<64;'));
  }), 'Remote input is bounded to at most twenty wheel lines per packet');

  await startApp();
  const beforeReverse = await evaluate(`(() => {
    scrollLocally(-65.25 * screen.clientHeight / terminal.rows);
    const sent = calls.input.slice();
    scrollLocally(3.25 * screen.clientHeight / terminal.rows);
    return sent;
  })()`);
  await wait(`(calls.input.join('').match(/\\x1b\\[<65;\\d+;\\d+M/g) || []).length >= 3`);
  await settle(200);
  const reversed = wheels(await evaluate('calls.input'));
  assert.equal(reversed.filter(event => event.startsWith('\x1b[<64;')).length, wheels(beforeReverse).length,
    'Changing direction discards only unsent old-direction debt');
  assert.equal(reversed.filter(event => event.startsWith('\x1b[<65;')).length, 3,
    'The new direction is not reduced by old-direction debt');

  await startApp();
  await evaluate('scrollLocally(-2000.25 * screen.clientHeight / terminal.rows)');
  await touch('touchStart',100,250);
  const afterNewTouch = await evaluate('calls.input');
  assert.ok(wheels(afterNewTouch).length < 2000, 'The new touch interrupts a still-pending large scroll');
  await settle();
  assert.deepEqual(await evaluate('calls.input'), afterNewTouch, 'A fresh touch discards unsent wheel debt');
  await touch('touchEnd',100,250);
  t.diagnostic('PASS: wheel batching preserves whole-line debt, clears it on reversal, and stops it on a new touch');

  await startApp();
  await evaluate('applicationScrollAckEwma = 400; scrollLocally(-200.25 * applicationScrollRowHeight())');
  assert.equal(wheels(await evaluate('calls.input')).length, 20, 'The first bounded batch starts immediately');
  await wait(`${wheelCount} === 80`);
  assert.equal(await evaluate('applicationScrollOutstandingRows()'), 80);
  assert.equal(await evaluate('applicationScrollBatches.size'), 4);
  await settle(50);
  assert.equal(wheels(await evaluate('calls.input')).length, 80,
    'The row window fills without waiting one RTT per batch, then holds at 80 rows');
  const receiptIds = await evaluate('[...applicationScrollBatches.keys()]');
  const awaitingLead = await evaluate('visualOffset');
  const awaitingLatency = await evaluate('applicationScrollAckEwma');
  await applyApplicationScreen();
  await applyApplicationScreen({lines:[{row:0,ansi:paint('Streaming status update')}]});
  await applyApplicationScreen({full:true,lines:[{row:0,ansi:paint('Resynced status')}]});
  assert.equal(wheels(await evaluate('calls.input')).length, 80,
    'Empty patches, streaming output and snapshots cannot refill transport credit');
  assert.deepEqual(await evaluate('[...applicationScrollBatches.keys()]'), receiptIds);
  assert.equal(await evaluate('visualOffset'), awaitingLead, 'Unrelated output cannot remove the local lead');
  assert.equal(await evaluate('applicationScrollAckEwma'), awaitingLatency, 'Unrelated output cannot train RPC latency');
  await evaluate(`window.tty7ScrollAcknowledged(${appGeneration - 1},${receiptIds[0]});
    window.tty7ScrollAcknowledged(${appGeneration},${receiptIds.at(-1) + 1})`);
  assert.equal(wheels(await evaluate('calls.input')).length, 80, 'Wrong generation and unknown IDs return no credit');
  const replyLatency = await evaluate(`(() => {
    applicationScrollAckEwma = 0;
    const elapsed = performance.now() - applicationScrollBatches.get(${receiptIds[2]}).sentAt;
    window.tty7ScrollAcknowledged(${appGeneration},${receiptIds[2]});
    return {elapsed,measured:applicationScrollAckEwma};
  })()`);
  assert.ok(Math.abs(replyLatency.elapsed - replyLatency.measured) < 1,
    'An out-of-order reply measures its own batch timestamp');
  await wait(`${wheelCount} === 100`);
  assert.equal(await evaluate('applicationScrollOutstandingRows()'), 80,
    'A 20-row reply returns exactly 20 rows of credit');
  assert.equal(await evaluate(`applicationScrollBatches.has(${receiptIds[0]})`), true,
    'An out-of-order reply cannot acknowledge another batch');
  await evaluate(`window.tty7ScrollAcknowledged(${appGeneration},${receiptIds[2]})`);
  await settle(40);
  assert.equal(wheels(await evaluate('calls.input')).length, 100, 'A duplicate reply cannot refill credit twice');
  t.diagnostic('PASS: bounded row pipelining preserves exact per-batch credit and rejects unrelated screen/duplicate/stale replies');

  await startApp();
  const pacing = await evaluate(`(() => {
    scrollLocally(-65 * applicationScrollRowHeight());
    const firstSentAt = applicationScrollSentAt;
    window.tty7ScrollAcknowledged(${appGeneration},[...applicationScrollBatches.keys()][0]);
    return {firstSentAt,packets:calls.input.length};
  })()`);
  assert.equal(pacing.packets, 1, 'An immediate reply cannot synchronously drain wheel debt');
  await wait(`${wheelCount} >= 40`);
  const secondSentAt = await evaluate('[...applicationScrollBatches.values()][0].sentAt');
  assert.ok(secondSentAt - pacing.firstSentAt >= 15,
    'Backlogged full batches retain at least 16 ms between sends, allowing timer rounding');
  await ackApplicationScroll();
  assert.equal(wheels(await evaluate('calls.input')).length, 65);
  assert.equal(await evaluate('visualOffset'), 0);
  await startApp();
  const finePacing = await evaluate(`(() => {
    scrollLocally(-4 * applicationScrollRowHeight());
    const firstSentAt = applicationScrollSentAt;
    scrollLocally(-4 * applicationScrollRowHeight());
    return firstSentAt;
  })()`);
  await wait(`${wheelCount} === 8`);
  const fineSecond = await evaluate('[...applicationScrollBatches.values()][1].sentAt');
  assert.ok(fineSecond - finePacing >= 31, 'Small moves keep a 32 ms coalescing interval');
  await ackApplicationScroll();
  t.diagnostic('PASS: full batches can advance every frame while small moves retain coalescing');

  for (const previousRtt of [0,40]) {
    await startApp();
    await evaluate(`(async () => {
      applicationScrollAckEwma = ${previousRtt};
      for (let move=0;move<16;move++) {
        if (move) await new Promise(resolve => setTimeout(resolve,16));
        scrollLocally(-(move === 15 ? 5 : 4) * applicationScrollRowHeight());
      }
    })()`);
    await wait(`${wheelCount} === 65`);
    assert.equal(await evaluate('applicationScrollBatches.size <= 16'), true);
    assert.equal(await evaluate('applicationScrollStalled'), false,
      'Neither a cold link nor a previously fast link stops before a 400 ms RTT reply can arrive');
    assert.equal(await evaluate('applicationScrollOutstandingRows()'), 65);
    await ackApplicationScroll();
  }
  t.diagnostic('PASS: timeout grace covers continuous drags on cold and previously faster links');

  await startApp();
  await evaluate('applicationScrollAckEwma = 400; scrollLocally(-200 * applicationScrollRowHeight())');
  await wait(`${wheelCount} === 80`);
  const obsoleteIds = await evaluate('[...applicationScrollBatches.keys()]');
  await evaluate('scrollLocally(40 * applicationScrollRowHeight())');
  assert.equal(wheels(await evaluate('calls.input')).length, 100,
    'A full old-direction window leaves one reserved 20-row batch for reversal');
  assert.ok(wheels(await evaluate('calls.input')).at(-1).startsWith('\x1b[<65;'));
  const reversalId = await evaluate('[...applicationScrollBatches.keys()].at(-1)');
  await evaluate('stopScrolling(); scrollLocally(40 * applicationScrollRowHeight())');
  await settle(50);
  assert.equal(wheels(await evaluate('calls.input')).length, 100,
    'A fresh touch cannot erase outstanding transport charges or grow the reserve');
  await evaluate('scrollLocally(-40 * applicationScrollRowHeight()); scrollLocally(40 * applicationScrollRowHeight())');
  await settle(40);
  assert.equal(await evaluate('applicationScrollOutstandingRows()'), 100,
    'Repeated reversals cannot grow the row window beyond its one fixed reserve');
  await evaluate(`window.tty7ScrollAcknowledged(${appGeneration},${obsoleteIds[0]})`);
  await wait(`${wheelCount} === 120`);
  assert.equal(await evaluate('applicationScrollOutstandingRows()'), 100);
  assert.ok(wheels(await evaluate('calls.input')).at(-1).startsWith('\x1b[<65;'),
    'A canceled batch reply refills transport capacity for the latest intent only');
  assert.equal(await evaluate(`applicationScrollBatches.has(${reversalId})`), true,
    'An obsolete reply cannot acknowledge another outstanding batch');
  await evaluate(`window.tty7ScrollAcknowledged(${appGeneration},${obsoleteIds[0]})`);
  await settle(40);
  assert.equal(wheels(await evaluate('calls.input')).length, 120);
  t.diagnostic('PASS: reversal has reserved credit, while cancellation and repeated direction changes cannot reset the transport bound');

  for (const direction of [-1, 1]) {
    await startApp();
    await evaluate(`scrollLocally(${direction} * applicationScrollRowHeight())`);
    const expiredBatch = await evaluate('[...applicationScrollBatches.keys()][0]');
    assert.equal(await evaluate('applicationScrollDebt'), 0);
    await wait('applicationScrollStalled && !visualSettle');
    await applyApplicationScreen({lines:[{row:0,ansi:paint('Unrelated late output')}]});
    assert.equal(await evaluate('applicationScrollStalled'), true);
    await evaluate(`scrollLocally(${-direction} * applicationScrollRowHeight())`);
    assert.equal(wheels(await evaluate('calls.input')).length, 2, 'A zero-debt reversal recovers in either direction');
    const reversedBatch = await evaluate('[...applicationScrollBatches.keys()].at(-1)');
    await evaluate(`window.tty7ScrollAcknowledged(${appGeneration},${expiredBatch})`);
    assert.equal(await evaluate(`applicationScrollBatches.has(${reversedBatch})`), true,
      'An expired obsolete reply can only reclaim its own transport charge');
    assert.equal(await evaluate('applicationScrollStalled'), false);
    await ackApplicationScroll();
    assert.equal(await evaluate('visualOffset'), 0);
  }
  t.diagnostic('PASS: zero-debt timeout reversals recover in both directions without confusing old and new receipts');

  await startApp();
  await evaluate('scrollLocally(-120.25 * applicationScrollRowHeight())');
  await wait('applicationScrollStalled && !visualSettle');
  assert.equal(wheels(await evaluate('calls.input')).length, 80, 'Missing input replies eventually hold the filled row window');
  await applyApplicationScreen();
  assert.equal(await evaluate('applicationScrollStalled'), true, 'A late screen cannot release the timeout hold');
  await ackApplicationScroll();
  assert.equal(wheels(await evaluate('calls.input')).length, 120, 'Matching late replies drain retained whole-row debt');
  assert.equal(await evaluate('applicationScrollStalled'), false);
  t.diagnostic('PASS: missing replies hold further sends, and actual late receipts recover the pipeline');

  await startApp();
  await evaluate('applicationScrollAckEwma = 400');
  for (let i=0;i<16;i++) {
    await evaluate('scrollLocally(-applicationScrollRowHeight())');
    await settle(33);
  }
  assert.equal(await evaluate('applicationScrollBatches.size'), 16);
  await evaluate('scrollLocally(-applicationScrollRowHeight())');
  await settle(40);
  assert.equal(wheels(await evaluate('calls.input')).length, 16,
    'Tiny wheel batches also have a request-count bound');
  const tinyId = await evaluate('[...applicationScrollBatches.keys()][0]');
  await evaluate(`window.tty7ScrollAcknowledged(${appGeneration},${tinyId})`);
  await wait(`${wheelCount} === 17`);
  assert.equal(await evaluate('applicationScrollBatches.size'), 16);
  t.diagnostic('PASS: a count limit also bounds small outstanding RPCs');

  await startApp();
  // Fill the viewport like the phone grid does: the mount must clip the lead
  // instead of adding scrollable overflow below the screen.
  const tallRows = Array.from({length:38},(_,i)=>({row:i,ansi:paint('row '+i)}));
  const tall = {cols:40, rows:38, epoch:String(appGeneration), revision:String(++appSequence),
    base_revision:null, cursor:{row:37,col:0,shape:'block',visible:true,blink:false}, alternate:true,
    modes:{app_cursor:false,app_keypad:false,bracketed_paste:true,mouse:true,sgr_mouse:true,alternate_scroll:true},
    lines:tallRows};
  const tallRendered = await evaluate('calls.rendered');
  await evaluate(`window.tty7Screen(${appGeneration},${JSON.stringify(tall)},${appSequence})`);
  await wait(`calls.rendered === ${tallRendered + 1}`);
  const rowHeight = await evaluate('applicationScrollRowHeight()');
  const liveScrollHeight = await evaluate('viewport.scrollHeight');
  await evaluate('scrollLocally(-applicationScrollRowHeight())');
  const leading = await evaluate('visualOffset');
  // A downward drag reveals older output, so the content leads downward.
  assert.ok(leading >= rowHeight - 0.5 && leading <= rowHeight + 0.5,
    'One unconfirmed row leads the rendered screen by one row');
  assert.match(await evaluate('screen.style.transform'), /translate3d\(/);
  assert.equal(await evaluate('viewport.scrollHeight'), liveScrollHeight,
    'The local lead does not extend the scrollable viewport');
  await ackApplicationScroll();
  assert.equal(await evaluate('visualOffset'), 0, 'The delivered input batch settles the temporary local lead');
  assert.equal(await evaluate('screen.style.transform'), '');
  t.diagnostic('PASS: the screen leads by undelivered rows and settles without moving the viewport');

  await startApp();
  await evaluate('scrollLocally(-0.5 * applicationScrollRowHeight())');
  assert.equal(await evaluate('visualOffset'), 0, 'Sub-row travel waits for a whole wheel line');
  assert.equal(wheels(await evaluate('calls.input')).length, 0);
  await evaluate('scrollLocally(-2 * applicationScrollRowHeight())');
  assert.ok(await evaluate('visualOffset') > 0);
  await evaluate('scrollLocally(applicationScrollRowHeight())');
  assert.ok(await evaluate('visualOffset') < 0, 'Reversing direction moves the local lead immediately');
  await evaluate('stopScrolling()');
  assert.equal(await evaluate('visualOffset'), 0);
  t.diagnostic('PASS: reversing direction replaces the local lead instead of fighting it');

  await startApp();
  await startFling();
  assert.ok(await evaluate('visualOffset') > 0,
    'A real downward drag leads the screen before the application answers');
  const keyFocus = await evaluate('calls.focus');
  const shortcut = '\x1b';
  const afterKey = await evaluate(`(() => {
    scrollLocally(-2000.25 * screen.clientHeight / terminal.rows);
    window.tty7Key(${appGeneration},${JSON.stringify(shortcut)});
    return calls.input.slice();
  })()`);
  assert.equal(afterKey.filter(text => text === shortcut).length, 1, 'The current shortcut is delivered once, verbatim');
  assert.equal(afterKey.at(-1), shortcut);
  assert.equal(await evaluate('calls.focus'), keyFocus);
  assert.equal(await evaluate('document.activeElement === input'), false, 'A native shortcut does not focus the IME');
  assert.equal(await evaluate('visualOffset'), 0, 'The shortcut removes the local lead');
  await settle();
  assert.deepEqual(await evaluate('calls.input'), afterKey, 'The shortcut cancels old wheel batches and inertia');

  await startApp();
  await evaluate(`scrollLocally(-65.25 * screen.clientHeight / terminal.rows);
    window.tty7Key(${appGeneration - 1},${JSON.stringify(shortcut)})`);
  for (let batch = 0; batch < 6 && wheels(await evaluate('calls.input')).length < 65; batch++) {
    await ackApplicationScroll();
    await settle(20);
  }
  await wait(`${wheelCount} >= 65`);
  await settle(150);
  const staleKeyInput = await evaluate('calls.input');
  assert.equal(wheels(staleKeyInput).length, 65, 'A stale shortcut cannot cancel scrolling in the current pane');
  assert.equal(staleKeyInput.includes(shortcut), false, 'A stale generation cannot send a shortcut');

  await startApp({synced:false});
  const unsyncedFocus = await evaluate('calls.focus');
  await evaluate(`window.tty7Key(${appGeneration},${JSON.stringify(shortcut)});
    window.tty7Synced(${appGeneration - 1});
    window.tty7Key(${appGeneration},${JSON.stringify(shortcut)})`);
  await settle(150);
  assert.deepEqual(await evaluate('calls.input'), [], 'A shortcut cannot bypass subscription sync');
  assert.equal(await evaluate('calls.focus'), unsyncedFocus);
  assert.equal(await evaluate('history.hidden && isAtLive()'), true, 'Ignored keys leave the current viewport unchanged');
  await evaluate(`window.tty7Synced(${appGeneration})`);
  await evaluate(`window.tty7Key(${appGeneration},${JSON.stringify(shortcut)})`);
  assert.deepEqual(await evaluate('calls.input'), [shortcut], 'The same shortcut works after the current pane is synced');
  t.diagnostic('PASS: native Esc stops current inertia, while stale and unsynced keys cannot affect the pane');

  // A fresh immutable server view contains its adjoining screen; a reopened
  // cache may only be reused while the live revision still matches.
  await startApp();
  await evaluate(`window.tty7AgentKind(${appGeneration},'omp'); showHistory()`);
  const coherentPage = [
    {cols:40,ansi:paint('retained end'),text:'retained end'},
    {cols:40,ansi:paint('captured screen'),text:'captured screen'},
  ];
  await evaluate(`window.tty7History(${appGeneration},{lines:${JSON.stringify(coherentPage)},cursor:'older-a',has_more:true,truncated:false})`);
  assert.match(await evaluate('historyText.textContent'), /captured screen/);
  const coherentRequests = await evaluate('calls.history');
  await evaluate('returnLive(); showHistory()');
  assert.equal(await evaluate('calls.history'),coherentRequests, 'An unchanged revision reuses its entire coherent snapshot');
  const nextLive = {...fullScreen, revision:'fresh-history-screen', epoch:String(appGeneration), base_revision:null, alternate:false};
  const previousRendered = await evaluate('calls.rendered');
  await evaluate(`returnLive(); window.tty7Screen(${appGeneration},${JSON.stringify(nextLive)},${++appSequence})`);
  await wait(`calls.rendered === ${previousRendered + 1}`);
  await evaluate('showHistory()');
  assert.equal(await evaluate('calls.history'),coherentRequests + 1, 'New live output refreshes the complete snapshot rather than attaching a new screen to an old suffix');
  assert.equal(await evaluate('historyText.childElementCount'),0, 'Old retained rows are absent while a fresh snapshot loads');
  await evaluate('returnLive(); showHistory()');
  assert.equal(await evaluate('calls.history'),coherentRequests + 1, 'Reopening a pending request does not overlap generation-only callbacks');
  await evaluate(`window.tty7History(${appGeneration},{lines:${JSON.stringify(coherentPage)},cursor:'older-b',has_more:true,truncated:false})`);
  await evaluate(`window.tty7AgentKind(${appGeneration},'codex')`);
  assert.equal(await evaluate('historyLive.hidden'),true, 'Codex keeps its native Esc instead of the history return button');
  await evaluate(`window.tty7AgentKind(${appGeneration - 1},'omp')`);
  assert.equal(await evaluate('historyLive.hidden'),true, 'An old pane cannot change the current agent action');
  const scrollablePage = Array.from({length:100}, (_,index) => ({
    cols:40,ansi:paint(`navigation ${index}`),text:`navigation ${index}`,
  }));
  await evaluate('resetHistory(); showHistory()');
  await evaluate(`window.tty7History(${appGeneration},{lines:${JSON.stringify(scrollablePage)},cursor:'navigation-older',has_more:true,truncated:false})`);
  for (const kind of ['omp', null]) {
    await evaluate(`window.tty7AgentKind(${appGeneration},${JSON.stringify(kind)}); showHistory()`);
    assert.equal(await evaluate('historyLive.hidden'),true, 'The current snapshot bottom has no redundant return action');
    await evaluate('scrollLocally(-200)');
    assert.equal(await evaluate('historyLive.hidden'),false, 'Scrolling away from the bottom reveals the return action');
    const beforeButton = await evaluate('({focus:calls.focus,input:calls.input.slice()})');
    const point = await evaluate(`(() => {const r=historyLive.getBoundingClientRect(); return {x:r.x+r.width/2,y:r.y+r.height/2}})()`);
    await touch('touchStart',point.x,point.y); await touch('touchEnd',point.x,point.y);
    await wait('history.hidden && following && isAtLive()');
    assert.deepEqual(await evaluate('({focus:calls.focus,input:calls.input.slice()})'),beforeButton, 'Return-to-bottom touch never opens the keyboard or sends PTY input');
    assert.equal(await evaluate('input.readOnly'),true);
    assert.equal(await evaluate('historyLive.hidden'),true, 'Returning live hides the action immediately');
  }
  await evaluate('showHistory()');
  const newerWhileReading = {...nextLive, revision:'output-while-at-history-bottom'};
  const renderedBeforeNewOutput = await evaluate('calls.rendered');
  await evaluate(`window.tty7Screen(${appGeneration},${JSON.stringify(newerWhileReading)},${++appSequence})`);
  await wait(`calls.rendered === ${renderedBeforeNewOutput + 1}`);
  assert.equal(await evaluate('historyLive.hidden'),true, 'Background output does not reveal a redundant action while the reader remains at the bottom');
  await evaluate('returnLive()');
  t.diagnostic('PASS: immutable snapshots replace loading screens; reopening refreshes changed revisions; OMP/Terminal return button is read-only while Codex keeps Esc');

  await evaluate('resetHistory(); showHistory()');
  for (let page = 0; page < 11; ++page) {
    if (page) await evaluate('requestHistory()');
    const start = 2200 - (page + 1) * 200;
    const pageRows = Array.from({length:200}, (_,i) => ({cols:40,ansi:paint(`bounded ${start+i}`),text:`bounded ${start+i}`}));
    await evaluate(`window.tty7History(${appGeneration},{lines:${JSON.stringify(pageRows)},cursor:'row-page-${page}',has_more:true,truncated:false})`);
  }
  assert.equal(await evaluate('historyText.childElementCount'),2000);
  assert.match(await evaluate('historyText.firstElementChild.textContent'),/bounded 0/);
  assert.match(await evaluate('historyText.lastElementChild.textContent'),/bounded 1999/);
  assert.deepEqual(await evaluate(`Array.from(historyText.children, row => Number(row.textContent.trim().slice('bounded '.length)))`),
    Array.from({length:2000}, (_,index) => index), 'Every retained numbered row remains in order with no interior holes');
  assert.equal(await evaluate('historyBoundary.hidden'),false, 'Evicted newer rows have an explicit navigable lower boundary');
  await evaluate('scrollLocally(100000)');
  assert.equal(await evaluate('history.hidden'),false, 'The lower window boundary is not falsely treated as live');
  assert.equal(await evaluate('historyLive.hidden'),false, 'A truncated window bottom is not the latest output');
  const beforeNewer = await evaluate('calls.history');
  await evaluate('document.getElementById("history-newer").click()');
  assert.equal(await evaluate('calls.history'),beforeNewer + 1, 'The lower boundary can reload a fresh newer window');
  assert.equal(await evaluate('historyText.childElementCount'),0);
  assert.equal(await evaluate('historyBoundary.hidden'),true);
  await evaluate(`window.tty7History(${appGeneration},{lines:${JSON.stringify(coherentPage)},cursor:'newest-again',has_more:true,truncated:false})`);
  assert.match(await evaluate('historyText.textContent'),/captured screen/);

  await evaluate('resetHistory(); showHistory()');
  for (let page = 0; page < 23; ++page) {
    if (page) await evaluate('requestHistory()');
    const pageRows = Array.from({length:10}, (_,i) => ({cols:40,ansi:paint(`bytes ${page}-${i}`),text:'x'.repeat(10000)}));
    await evaluate(`window.tty7History(${appGeneration},{lines:${JSON.stringify(pageRows)},cursor:'byte-page-${page}',has_more:true,truncated:false})`);
  }
  assert.ok(await evaluate('historyBytes <= maxHistoryBytes && historyText.childElementCount < maxHistoryRows'));
  assert.equal(await evaluate('historyBoundary.hidden'),false, 'The UTF-8 byte cap exposes the same browsable boundary as the row cap');
  await evaluate('requestHistory()');
  await evaluate(`window.tty7History(${appGeneration},{lines:[],cursor:null,has_more:false,truncated:true}); history.scrollTop=0; updateHistoryStatus()`);
  const oldestRequests = await evaluate('calls.history');
  await evaluate('scrollLocally(-10000); requestHistory()');
  assert.equal(await evaluate('calls.history'),oldestRequests, 'Reaching the server retained boundary does not retry unavailable older rows');
  t.diagnostic('PASS: row/byte eviction preserves a bounded contiguous window with explicit reload-newer navigation and a distinct oldest retained boundary');

  // A callback already awaiting the parser must not populate the next pane.
  await evaluate(`resetHistory(); showHistory(); window.lateHistory = window.tty7History(${appGeneration},{lines:${JSON.stringify(coherentPage)},cursor:null,has_more:false,truncated:false}); window.tty7Begin(${++appGeneration})`);
  await evaluate('window.lateHistory');
  assert.equal(await evaluate('historyText.childElementCount'),0);
  assert.equal(await evaluate('history.hidden'),true);
  await call('Page.navigate', {url:url + '?theme=dark'});
  await wait('typeof window.tty7Appearance === "function" && document.documentElement.dataset.theme === "dark"');
  assert.equal(await evaluate('terminal.options.theme.background'), '#1e1e20', 'Saved native theme applies on initial page load');
  ws.close();
});
