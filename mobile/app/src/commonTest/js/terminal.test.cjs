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
  await evaluate(`window.calls={focus:0,history:0,input:[],grids:[],rendered:0,begun:false}; window.Tty7={ready(){},resize(cols,rows){calls.grids.push([cols,rows])},begun(){calls.begun=true},rendered(){calls.rendered++},failed(g,m){throw new Error(m)},history(){calls.history++},focusInput(){calls.focus++},input(g,t){calls.input.push(t)}}; window.tty7Begin(1)`);
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
    await evaluate(`calls.input = []; ${synced ? `window.tty7Synced(${appGeneration})` : ''}`);
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


  await startApp();
  await evaluate('scrollLocally(-65.25 * screen.clientHeight / terminal.rows)');
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
  await startFling();
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
  await settle();
  assert.deepEqual(await evaluate('calls.input'), afterKey, 'The shortcut cancels old wheel batches and inertia');

  await startApp();
  await evaluate(`scrollLocally(-65.25 * screen.clientHeight / terminal.rows);
    window.tty7Key(${appGeneration - 1},${JSON.stringify(shortcut)})`);
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
