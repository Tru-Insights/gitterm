// Trusted preflight, evaluated before any agent HTML enters preview Chrome.
// A browser update that misses native initialization must fail closed.
(() => new Promise((resolve) => {
  const id = 'gitterm-guard-' + Math.random();
  const frame = document.createElement('iframe');
  frame.setAttribute('sandbox', 'allow-scripts');
  const guarded = "window.__gittermHtmlGuard === true && typeof RTCPeerConnection === 'undefined' && typeof webkitRTCPeerConnection === 'undefined'";
  const nested = '<script>parent.parent.postMessage({id:' + JSON.stringify(id) + ',scope:"nested",ok:(' + guarded + ')},"*")<\/script>';
  const quote = (text) => JSON.stringify(text).replace(/</g, '\\u003c');
  frame.srcdoc = '<meta http-equiv="Content-Security-Policy" content="default-src \'none\'; script-src \'unsafe-inline\'; frame-src \'none\'"><script>parent.postMessage({id:' + JSON.stringify(id) + ',scope:"root",ok:(' + guarded + ')},"*");const f=document.createElement("iframe");f.srcdoc=' + quote(nested) + ';document.documentElement.appendChild(f)<\/script>';
  const seen = {};
  let timer;
  const finish = (ok) => { clearTimeout(timer); window.removeEventListener('message', receive); frame.remove(); resolve(ok); };
  const receive = (event) => {
    const d = event.data;
    if (!d || d.id !== id || !['root','nested'].includes(d.scope)) return;
    seen[d.scope] = d.ok === true;
    if (d.ok !== true) finish(false);
    else if (seen.root && seen.nested) finish(true);
  };
  window.addEventListener('message', receive);
  timer = setTimeout(() => finish(false), 1500);
  document.body.appendChild(frame);
}))()
