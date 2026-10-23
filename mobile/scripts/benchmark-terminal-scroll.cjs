// Controlled Chromium comparison of terminal.js implementations. This uses
// real xterm DOM rendering and simulated transport/app delays, not a live Codex
// process or an Android device. Run with Node 22+:
// node mobile/scripts/benchmark-terminal-scroll.cjs /path/to/baseline-terminal.js /path/to/results.json [/path/to/serial-terminal.js]
'use strict';
const fs = require('node:fs');
const os = require('node:os');
const path = require('node:path');
const {spawn} = require('node:child_process');
const {pathToFileURL} = require('node:url');
const {createHash} = require('node:crypto');

const baseline = process.argv[2];
if (!baseline) throw new Error('Provide a baseline terminal.js path');
const output = process.argv[3];
const serial = process.argv[4];
// Rerun just one unchanged model/version without recomputing historical data.
const only = process.env.TTY7_BENCH_ONLY;
if (only && !['before','serial','after'].includes(only)) throw new Error('Invalid TTY7_BENCH_ONLY');
if (only === 'serial' && !serial) throw new Error('Provide the serial implementation path');
const assets = path.resolve(__dirname, '../app/src/androidMain/assets/terminal');
const browser = [process.env.TTY7_TEST_BROWSER,
  '/Applications/Google Chrome.app/Contents/MacOS/Google Chrome',
  '/Applications/Microsoft Edge.app/Contents/MacOS/Microsoft Edge',
  '/usr/bin/chromium', '/usr/bin/chromium-browser', '/usr/bin/google-chrome',
].find(candidate => candidate && fs.existsSync(candidate));
if (!browser) throw new Error('Set TTY7_TEST_BROWSER to a Chromium executable');

// Installed in the page after terminal.js and the bundled fonts have loaded.
async function runTrial({rtt, paintMs, workload, generation}) {
  const timers = new Set();
  let maxTimerLagMs = 0;
  const later = (fn, delay) => {
    const scheduledAt = performance.now() + Math.max(0,delay);
    const id = setTimeout(() => {
      maxTimerLagMs = Math.max(maxTimerLagMs,performance.now() - scheduledAt);
      timers.delete(id); fn();
    }, delay);
    timers.add(id);
  };
  const sleep = ms => new Promise(resolve => later(resolve, ms));
  const until = async test => {
    const end = performance.now() + 2000;
    while (!test()) {
      if (performance.now() > end) throw new Error('Benchmark renderer did not become ready');
      await sleep(2);
    }
  };
  const cols = 40, rows = 24;
  const paint = text => Array.from(text.padEnd(cols)).map((c,i) => `\x1b[${i+1}G${c}`).join('');
  const modes = {app_cursor:false,app_keypad:false,bracketed_paste:true,mouse:true,sgr_mouse:true,alternate_scroll:true};
  let begun = false, revision = 0, applied = 0, position = 0, serverAvailable = 0;
  let producedPosition = 0;
  let outstanding = 0, maxOutstanding = 0, appPending = 0, maxAppPending = 0;
  let outstandingRows = 0, maxOutstandingRows = 0;
  let active = false, started = 0, firstPaint = null, finalPaint = null, resolveTarget;
  let heartbeat = 0, lastFrameAt = 0, maxAnimationFrameGapMs = 0;
  const trackFrames = now => {
    maxAnimationFrameGapMs = Math.max(maxAnimationFrameGapMs,now - lastFrameAt);
    lastFrameAt = now;
    heartbeat = requestAnimationFrame(trackFrames);
  };
  const target = workload === 'boundary-reverse' ? 1 : 65;
  const screens = [];
  const inputs = [];
  const targetPainted = new Promise(resolve => { resolveTarget = resolve; });
  const drain = () => {
    if (!screens.length || writing) return;
    const state = screens.shift();
    state.base_revision = appliedRevision;
    window.tty7Screen(generation, state, Number(state.revision));
  };
  const makeScreen = (full = false) => {
    ++revision;
    // Match the gateway's screen_parts: unchanged rows are omitted, including
    // the transcript during status-only output. Repainting it every 20ms would
    // add artificial body damage to this latency comparison.
    const lines = full ? Array.from({length:rows},(_,row) =>
      ({row,ansi:paint(row ? `Transcript row ${row}` : `position ${position}`)})) : [];
    if (!full) {
      if (position !== producedPosition) lines.push({row:0,ansi:paint(`position ${position}`)});
      lines.push({row:23,ansi:paint(`status revision ${revision}`)});
    }
    producedPosition = position;
    return {cols,rows,epoch:String(generation),revision:String(revision),base_revision:null,
      alternate:true,cursor:{row:23,col:0,shape:'block',visible:true,blink:false},modes,lines};
  };
  const emitScreen = () => {
    const state = makeScreen();
    later(() => { screens.push(state); drain(); }, rtt / 2);
  };
  const send = (gen, batch, text) => {
    if (!active || gen !== generation) return;
    const wheels = text.match(/\x1b\[<6[45];\d+;\d+M/g) || [];
    if (!wheels.length) return;
    const delta = (wheels[0].startsWith('\x1b[<64;') ? 1 : -1) * wheels.length;
    inputs.push({at:performance.now() - started,rows:delta,batch});
    maxOutstanding = Math.max(maxOutstanding, ++outstanding);
    outstandingRows += wheels.length;
    maxOutstandingRows = Math.max(maxOutstandingRows,outstandingRows);
    later(() => {
      // The input RPC replies when the PTY write succeeds; application paint
      // occurs independently and is deliberately not treated as an RPC ACK.
      later(() => {
        outstanding--;
        outstandingRows -= wheels.length;
        if (batch !== null) window.tty7ScrollAcknowledged(gen, batch);
      }, rtt / 2);
      maxAppPending = Math.max(maxAppPending, ++appPending);
      const now = performance.now();
      serverAvailable = Math.max(now, serverAvailable) + paintMs;
      later(() => {
        appPending--;
        const previous = position;
        position = Math.max(0, position + delta);
        if (position !== previous) emitScreen();
      }, serverAvailable - now);
    }, rtt / 2);
  };
  window.Tty7 = {ready(){},resize(){},focusInput(){},history(){},
    begun(){begun = true;},failed(g,message){throw new Error(message);},
    input(g,text){send(g,null,text);},scroll(g,batch,text){send(g,batch,text);},
    rendered(){applied++;drain();},
  };
  const render = terminal.onRender(() => {
    if (!active) return;
    const text = screen.querySelector('.xterm-rows').textContent;
    const match = text.match(/position (\d+)/);
    if (!match) return;
    const visible = Number(match[1]);
    const elapsed = performance.now() - started;
    if (visible && firstPaint === null) firstPaint = elapsed;
    if (visible === target && finalPaint === null) { finalPaint = elapsed; resolveTarget(); }
  });
  try {
    window.tty7Begin(generation);
    await until(() => begun);
    window.tty7Screen(generation, makeScreen(true), 1);
    await until(() => applied === 1);
    window.tty7Synced(generation);
    applicationScrollAckEwma = 0; // Identical cold latency estimate per trial.
    await sleep(40); // Complete the bootstrap DOM render before timing.
    active = true;
    started = performance.now();
    lastFrameAt = started;
    heartbeat = requestAnimationFrame(trackFrames);
    if (workload === 'boundary-reverse') {
      scrollLocally(applicationScrollRowHeight()); // At the latest boundary: no output.
      await sleep(Math.max(350, rtt + 100));
      started = performance.now(); // Measure only response to the reversal.
      scrollLocally(-applicationScrollRowHeight());
    } else {
      if (workload === 'streaming' || workload === 'streaming-drag') {
        const status = () => {
          emitScreen(); // A new revision with no transcript movement.
          later(status, 20);
        };
        status();
      }
      if (workload === 'drag' || workload === 'streaming-drag') {
        // 65 rows across sixteen move frames over 240ms, not one giant delta.
        for (let move=0;move<16;move++) {
          if (move) await sleep(16);
          scrollLocally(-(move === 15 ? 5 : 4) * applicationScrollRowHeight());
        }
      } else scrollLocally(-65 * applicationScrollRowHeight());
    }
    await Promise.race([targetPainted, sleep(2200)]);
    await sleep(50);
    maxAnimationFrameGapMs = Math.max(maxAnimationFrameGapMs,performance.now() - lastFrameAt);
    return {rtt,paintMs,workload,completed:finalPaint !== null,firstPaintMs:firstPaint,completeMs:finalPaint,
      maxOutstandingRpc:maxOutstanding,maxOutstandingRows,maxApplicationBatches:maxAppPending,maxTimerLagMs,maxAnimationFrameGapMs,
      finalPosition:position,buffer:terminal.buffer.active.getLine(0).translateToString(true),
      dom:screen.querySelector('.xterm-rows').textContent.slice(0,cols),visibility:document.visibilityState,inputs};
  } finally {
    active = false;
    for (const id of timers) clearTimeout(id);
    cancelAnimationFrame(heartbeat);
    render.dispose();
    stopScrolling();
  }
}

async function compareVersion(name, script) {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), 'tty7-scroll-benchmark-'));
  const mount = path.join(root, 'terminal');
  fs.mkdirSync(mount);
  for (const file of fs.readdirSync(assets)) {
    if (file !== 'terminal.js') fs.symlinkSync(path.join(assets, file), path.join(mount,file));
  }
  fs.copyFileSync(script, path.join(mount, 'terminal.js'));
  const url = pathToFileURL(path.join(mount, 'index.html')).href;
  const chrome = spawn(browser, ['--headless=new','--disable-gpu','--no-first-run','--no-default-browser-check',
    '--disable-background-timer-throttling','--disable-renderer-backgrounding','--disable-backgrounding-occluded-windows',
    '--remote-debugging-port=0',`--user-data-dir=${path.join(root,'profile')}`,url], {stdio:['ignore','ignore','pipe']});
  let ws;
  try {
    const endpoint = await new Promise((resolve,reject) => {
      let log = '';
      const timer = setTimeout(() => reject(new Error('Browser startup timed out')),10000);
      chrome.once('error',error => {clearTimeout(timer);reject(error);});
      chrome.stderr.on('data', data => {
        log += data;
        const match = log.match(/DevTools listening on ws:\/\/([^/]+)/);
        if (match) {clearTimeout(timer);resolve(`http://${match[1]}`);}
      });
    });
    let page;
    for (let n=0;n<100 && !page;n++) {
      page = (await (await fetch(`${endpoint}/json/list`)).json()).find(p => p.url === url);
      if (!page) await new Promise(resolve => setTimeout(resolve,20));
    }
    if (!page) throw new Error('Benchmark page not found');
    ws = new WebSocket(page.webSocketDebuggerUrl);
    await new Promise(resolve => ws.addEventListener('open',resolve,{once:true}));
    let nextId = 0;
    const requests = new Map();
    ws.addEventListener('message',({data}) => {
      const message = JSON.parse(data), pending = requests.get(message.id);
      if (!pending) return;
      clearTimeout(pending.timer);requests.delete(message.id);
      if (message.error) pending.reject(message.error);else pending.resolve(message.result);
    });
    const call = (method,params = {}) => new Promise((resolve,reject) => {
      const id = ++nextId;
      const timer = setTimeout(() => {requests.delete(id);reject(new Error(`CDP timed out: ${method}`));},10000);
      requests.set(id,{resolve,reject,timer});ws.send(JSON.stringify({id,method,params}));
    });
    const evaluate = async expression => {
      const result = await call('Runtime.evaluate',{expression,returnByValue:true,awaitPromise:true});
      if (result.exceptionDetails) throw new Error(JSON.stringify(result.exceptionDetails));
      return result.result.value;
    };
    await call('Emulation.setDeviceMetricsOverride',{width:390,height:700,deviceScaleFactor:1,mobile:true});
    await call('Page.bringToFront');
    for (let n=0;n<100;n++) {
      if (await evaluate('typeof window.tty7Begin === "function" && fontsReady')) break;
      await new Promise(resolve => setTimeout(resolve,20));
    }
    const trials = [], discardedTrials = [];
    let generation = 0;
    for (const rtt of [40,160,400]) {
      for (const workload of ['quiet','streaming','drag','streaming-drag','boundary-reverse']) {
        for (let repeat=0;repeat<3;repeat++) {
          let accepted = false;
          for (let attempt=0;attempt<3 && !accepted;attempt++) {
            await call('Page.bringToFront');
            const config = {rtt,paintMs:24,workload,generation:++generation};
            const trial = await evaluate(`(${runTrial.toString()})(${JSON.stringify(config)})`);
            if (trial.maxTimerLagMs <= 100 && trial.maxAnimationFrameGapMs <= 100) {
              trials.push(trial);accepted = true;
            } else discardedTrials.push({rtt,workload,maxTimerLagMs:trial.maxTimerLagMs,
              maxAnimationFrameGapMs:trial.maxAnimationFrameGapMs});
          }
          if (!accepted) throw new Error(`Unable to obtain an unthrottled sample: ${name} ${rtt}ms ${workload}`);
        }
        process.stderr.write(`${name}: RTT ${rtt}ms ${workload} complete\n`);
      }
    }
    return {name,browser:await call('Browser.getVersion'),
      sha256:createHash('sha256').update(fs.readFileSync(path.join(mount,'terminal.js'))).digest('hex'),trials,discardedTrials};
  } finally {
    ws?.close();
    if (chrome.exitCode === null) {chrome.kill();await new Promise(resolve => chrome.once('exit',resolve));}
    fs.rmSync(root,{recursive:true,force:true});
  }
}

(async () => {
  const results = {environment:'Headless Chromium, 390x700 mobile emulation, real bundled xterm DOM renderer',
    model:'Symmetric simulated transport RTT; input RPC ACK at PTY write; 24ms per-batch application paint; delta-row status updates every 20ms; drag moves every 16ms for 240ms',
    repetitions:3,timeoutMs:2200,maxAcceptedTimerLagMs:100,maxAcceptedAnimationFrameGapMs:100,versions:[]};
  if (!only || only === 'before') results.versions.push(await compareVersion('before',path.resolve(baseline)));
  if (serial && (!only || only === 'serial')) results.versions.push(await compareVersion('serial',path.resolve(serial)));
  if (!only || only === 'after') results.versions.push(await compareVersion('after',path.join(assets,'terminal.js')));
  const json = JSON.stringify(results,null,2) + '\n';
  if (output) fs.writeFileSync(output,json);
  process.stdout.write(json);
})().catch(error => {process.stderr.write(`${error.stack || error}\n`);process.exitCode = 1;});
