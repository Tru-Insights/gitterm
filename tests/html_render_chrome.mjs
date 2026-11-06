// Run: node tests/html_render_chrome.mjs. Uses a temporary headless profile.
import { access, mkdtemp, readFile, readdir, rm } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { spawn } from 'node:child_process';
import { once } from 'node:events';
import { createServer } from 'node:http';
import assert from 'node:assert/strict';
const installHint = 'npx @puppeteer/browsers install chrome-headless-shell';
async function findChrome() {
  if (process.env.GITTERM_V6_CHROME_PATH) return process.env.GITTERM_V6_CHROME_PATH;
  const root = join(process.env.HOME, '.cache/puppeteer/chrome-headless-shell');
  let versions;
  try { versions = await readdir(root); } catch (error) {
    if (error.code !== 'ENOENT') throw error;
    return null;
  }
  versions.sort((a, b) => b.localeCompare(a, undefined, { numeric: true }));
  for (const version of versions) {
    const candidate = join(root, version, 'chrome-headless-shell-mac-arm64', 'chrome-headless-shell');
    try { await access(candidate); return candidate; } catch (error) {
      if (error.code !== 'ENOENT') throw error;
    }
  }
  return null;
}
const chrome = await findChrome();
if (!chrome) {
  console.log(`SKIP: no Chrome binary found; set GITTERM_V6_CHROME_PATH or install one with: ${installHint}`);
  process.exit(0);
}
let received = 0; const server = createServer((req,res) => { res.end('unexpected'); }); server.on('connection',()=>received++); server.listen(0, '127.0.0.1'); await once(server,'listening');
const address = '127.0.0.1:' + server.address().port;
const profile = await mkdtemp(join(tmpdir(), 'gitterm-v6-html-test-'));
const sandboxArgs = process.env.CODEX_SANDBOX ? ['--no-sandbox', '--single-process'] : [];
const child = spawn(chrome, [...sandboxArgs, '--headless=new', '--disable-features=IsolateSandboxedIframes', '--no-first-run', '--no-default-browser-check', '--disable-background-networking', '--remote-debugging-address=127.0.0.1', '--remote-debugging-port=0', '--user-data-dir=' + profile, 'about:blank'], { stdio: 'ignore' });
const delay = (ms) => new Promise((r) => setTimeout(r, ms));
let socket;
try {
  let port;
  for (let i = 0; i < 150 && !port; i++) { try { port = (await readFile(join(profile, 'DevToolsActivePort'), 'utf8')).split('\n')[0]; } catch (_) { await delay(100); } }
  assert.ok(port, 'Chrome did not start');
  const target = (await (await fetch(`http://127.0.0.1:${port}/json/list`)).json()).find((t) => t.type === 'page');
  socket = new WebSocket(target.webSocketDebuggerUrl); await once(socket, 'open');
  let next = 0; const pending = new Map();
  socket.addEventListener('message', (e) => { const m = JSON.parse(e.data); if (m.id) { const p = pending.get(m.id); if (!p) return; pending.delete(m.id); m.error ? p.reject(new Error(JSON.stringify(m.error))) : p.resolve(m.result); } });
  const call = (method, params = {}) => new Promise((resolve, reject) => { const id = ++next; pending.set(id, { resolve, reject }); socket.send(JSON.stringify({ id, method, params })); setTimeout(() => { if (pending.delete(id)) reject(new Error(method + ' timed out')); }, 10000).unref(); });
  const evaluate = async (expression) => { const r = await call('Runtime.evaluate', { expression, awaitPromise: true, returnByValue: true }); if (r.exceptionDetails) throw new Error(JSON.stringify(r.exceptionDetails)); return r.result.value; };
  await call('Page.enable'); await call('Runtime.enable'); await call('Network.enable');
  await call('Emulation.setDeviceMetricsOverride', { width: 1000, height: 900, deviceScaleFactor: 1, mobile: false });
  const guard = await readFile('assets/html_frame_guard.js', 'utf8');
  const guardScript = await call('Page.addScriptToEvaluateOnNewDocument', { source: guard });
  await evaluate(guard);
  let page = await readFile('assets/agent_chat.html', 'utf8');
  const vendor = await Promise.all(['assets/vendor/marked.min.js','assets/vendor/highlight.min.js','assets/html_render.js'].map((p) => readFile(p, 'utf8')));
  page = page.replace('/*__VENDOR_JS__*/', () => vendor.join('\n;\n')).replace('/*__BAR_HINT__*/','');
  page = page.replace('</body>', () => '<script>window.__testMode="chrome"; window.__gittermCanRenderHtml=true; window.__nativeCalls=[]; window.ipc={postMessage:()=>{}}; window.__gittermNativePost=(s)=>window.__nativeCalls.push(JSON.parse(s));</script><script>' + '</script></body>');
  const frame = (await call('Page.getFrameTree')).frameTree.frame.id;
  await call('Page.setDocumentContent', { frameId: frame, html: page });
  assert.equal(await evaluate(await readFile('assets/html_guard_probe.js','utf8')), true, 'native frame-guard preflight');
  await call('Runtime.evaluate', { expression: (await readFile('tests/html_render_ui.js', 'utf8')).replaceAll('127.0.0.1:1', address), awaitPromise: false });
  for (let i = 0; i < 100 && !await evaluate('!!window.__awaitTrustedClick || !!window.__testResult'); i++) await delay(100);
  if (await evaluate('!!window.__awaitTrustedClick')) {
    await call('Input.dispatchMouseEvent', { type: 'mousePressed', x: 30, y: 110, button: 'left', clickCount: 1 });
    await call('Input.dispatchMouseEvent', { type: 'mouseReleased', x: 30, y: 110, button: 'left', clickCount: 1 });
  }
  for (let i = 0; i < 50 && !await evaluate('!!window.__testResult'); i++) await delay(100);
  const result = await evaluate('window.__testResult');
  assert.ok(result, 'UI tests timed out');
  console.log(JSON.stringify(result, null, 2));
  assert.equal(result.failures.length, 0);
  assert.deepEqual(await evaluate('window.__nativeCalls.filter((c)=>c.type==="open_url").map((c)=>c.url)'), ['https://example.com/visual']);
  assert.equal(received, 0, 'sandboxed content reached the network');
  const screenshot = await call('Page.captureScreenshot', { format: 'png' });
  const { writeFile } = await import('node:fs/promises');
  await writeFile(join(tmpdir(), 'gitterm-v6-html-render.png'), Buffer.from(screenshot.data, 'base64'));
  // Without the native capability, replay is inert (phone/browser fallback).
  await evaluate('window.__resetConversation(); delete window.__gittermNativePost; window.__appendEvent({kind:"harness",event:{type:"item_started",data:{id:"phone",kind:{name:"html_render",input:{html:"<script>throw 1<\\/script>",height:100}}}}});window.__appendEvent({kind:"harness",event:{type:"item_completed",data:{id:"phone",output:JSON.stringify({html_render:{html:"<h1>phone</h1>",height:100}}),is_error:false}}});');
  assert.equal(await evaluate('document.querySelectorAll(".html-render iframe").length'), 0);
  await evaluate('window.__gittermCanRenderHtml=false;window.__gittermNativePost=()=>{};window.__resetConversation();window.__appendEvent({kind:"harness",event:{type:"item_started",data:{id:"unsupported",kind:{name:"html_render",input:{}}}}});window.__appendEvent({kind:"harness",event:{type:"item_completed",data:{id:"unsupported",output:JSON.stringify({html_render:{html:"<p>inert</p>",height:100}}),is_error:false}}});');
  assert.equal(await evaluate('document.querySelectorAll(".html-render iframe").length'),0,'unsupported native platform replay is inert');
  await call('Page.removeScriptToEvaluateOnNewDocument', {identifier:guardScript.identifier});
  assert.equal(await evaluate(await readFile('assets/html_guard_probe.js','utf8')), false, 'preflight fails without native initialization');
  await evaluate(`window.__unguardedRan=false;addEventListener('message',e=>{if(e.data==='unguarded-code-ran')window.__unguardedRan=true;});window.unguarded=GitTermHtml.create('<script>parent.postMessage("unguarded-code-ran","*")<\\/script>',100,'dark');document.body.appendChild(unguarded.frame);`);
  await delay(200);
  assert.equal(await evaluate('unguarded.state.guardError'),true,'missing guard is observable');
  assert.equal(await evaluate('window.__unguardedRan'),false,'missing guard never executes agent HTML');
  console.log('Chrome HTML sandbox/UI checks passed. Screenshot: ' + join(tmpdir(), 'gitterm-v6-html-render.png'));
} finally {
  server.close();
  if (socket) socket.close();
  if (child.exitCode === null && child.signalCode === null) { const exited = once(child, 'exit'); child.kill(); await exited; }
  await rm(profile, { recursive: true, force: true });
}
