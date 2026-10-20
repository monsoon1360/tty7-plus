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
  assert.match(await evaluate('historyScreen.textContent'), /Current screen/);
  assert.equal(await evaluate('history.hidden'), false);
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
  await evaluate(`window.tty7History(1,{lines:[${JSON.stringify(line)}],cursor:'older',has_more:true,truncated:false})`);
  assert.match(await evaluate('historyText.textContent'), /Older retained line/);
  assert.match(await evaluate('historyScreen.textContent'), /Current screen/);
  assert.equal(await evaluate('history.hidden'),false);
  await evaluate('requestHistory(); window.tty7HistoryError(1,"模拟加载失败")');
  assert.match(await evaluate('historyScreen.textContent'), /Current screen/);
  await evaluate('returnLive()');
  assert.equal(await evaluate('history.hidden'),true);
  t.diagnostic('PASS: retained history and error recovery keep the current screen available');

  // Use a shared desktop grid taller than the phone, so leaving history must
  // also restore the outer live viewport to its actual lower boundary.
  state.revision = '3';
  state.rows = 70;
  await evaluate(`window.tty7Screen(1,${JSON.stringify(state)},3)`);
  await wait('calls.rendered === 3');
  await evaluate('showHistory()');
  const lines = Array.from({length:250}, (_,index) => ({
    cols:40,ansi:paint(`History row ${index}`),text:`History row ${index}`,
  }));
  await evaluate(`window.tty7History(1,{lines:${JSON.stringify(lines)},cursor:'oldest',has_more:true,truncated:false})`);
  const retainedText = await evaluate('historyText.textContent + historyScreen.textContent');
  const retainedRequests = await evaluate('calls.history');
  for (const dark of [true, false]) {
    await evaluate(`window.tty7Appearance(${dark})`);
    assert.equal(await evaluate('history.hidden'), false, 'Theme changes keep history open');
    assert.equal(await evaluate('historyText.textContent + historyScreen.textContent'), retainedText);
    assert.equal(await evaluate('calls.history'), retainedRequests, 'Theme changes reuse cached history');
    assert.equal(await evaluate(`(() => {
      const hex = rgb => '#' + rgb.match(/\\d+/g).slice(0,3).map(n => Number(n).toString(16).padStart(2,'0')).join('');
      return [...historyText.querySelectorAll('span'), ...historyScreen.querySelectorAll('span')].every(span => {
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
  assert.notEqual(await evaluate('historyStatus.textContent'),'已到历史起点', 'An exhausted page does not imply the viewport is at the top');
  await evaluate('history.scrollTop = 0');
  await wait('historyStatus.textContent === "已到历史起点"');
  await evaluate('scrollLocally(300)');
  assert.notEqual(await evaluate('historyStatus.textContent'),'已到历史起点', 'The start label disappears when scrolling away from the top');

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

  // Native scroll events (for example a scrollbar or accessibility scroll)
  // follow the same boundary rule, while opening cached history stays open.
  await evaluate('showHistory()');
  await new Promise(resolve => setTimeout(resolve, 50));
  assert.equal(await evaluate('history.hidden'),false);
  await evaluate('scrollLocally(-300)');
  await evaluate('history.scrollTop = history.scrollHeight');
  await wait('history.hidden && following && isAtLive()');
  t.diagnostic('PASS: history start follows viewport position; dragging or scrolling to the bottom restores live mode and tap input');

  await evaluate('window.tty7Begin(2)');
  assert.equal(await evaluate('historyScreen.childElementCount'),0);
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
  assert.equal(await evaluate('history.hidden'),false, 'Returning to the main screen restores paged terminal history');
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
  await call('Page.navigate', {url:url + '?theme=dark'});
  await wait('typeof window.tty7Appearance === "function" && document.documentElement.dataset.theme === "dark"');
  assert.equal(await evaluate('terminal.options.theme.background'), '#1e1e20', 'Saved native theme applies on initial page load');
  ws.close();
});
