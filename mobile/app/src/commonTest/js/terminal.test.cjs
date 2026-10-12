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
  {skip: !browser && 'Chromium is unavailable; set TTY7_TEST_BROWSER', timeout:30000}, async t => {
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
  await evaluate(`window.calls={focus:0,history:0,input:[],rendered:0,begun:false}; window.Tty7={ready(){},resize(){},begun(){calls.begun=true},rendered(){calls.rendered++},failed(g,m){throw new Error(m)},history(){calls.history++},focusInput(){calls.focus++},input(g,t){calls.input.push(t)}}; window.tty7Begin(1)`);
  await wait('calls.begun');
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
  await call('Input.insertText', {text:'test input'});
  assert.deepEqual(await evaluate('calls.input'), ['test input']);
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
  await evaluate('window.tty7Begin(2)');
  assert.equal(await evaluate('historyScreen.childElementCount'),0);
  assert.equal(await evaluate('input.readOnly'),true);
  t.diagnostic('PASS: switching terminal clears history snapshot and input focus');
  ws.close();
});
