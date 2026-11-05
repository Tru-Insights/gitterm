// Runs against the real chat HTML in Chrome/CDP and the native Wry smoke.
(async () => {
  const checks = [];
  const check = (name, ok) => checks.push({ name, ok: !!ok });
  const wait = (ms) => new Promise((resolve) => setTimeout(resolve, ms));
  const H = (type, data) => ({ kind: 'harness', event: { type, data } });
  const prompt = { type: 'user_prompt', text: 'Show a visual' };
  const input = { html: '<style>body{padding:20px}h1{color:var(--accent)}</style><h1>A visual reply</h1><p>Theme-aware, isolated HTML.</p>', height: 240 };
  const start = (id, name, args) => H('item_started', { id, kind: { name, input: args } });
  const done = (id, error = false) => H('item_completed', { id, output: ['visual','bad'].includes(id) ? JSON.stringify({html_render: input}) : 'ok', is_error: error });
  const end = H('turn_completed', { status: { state: 'completed' }, usage: {}, cost_usd: null });
  const events = [prompt, H('turn_started', {}), start('visual', 'mcp__gitterm_tasks__html_render', input), done('visual'), start('later', 'Read', {}), done('later'), H('text_delta', 'Final reply'), end];
  const send = (evs) => evs.forEach(window.__appendEvent);
  try {
    window.__setTabId(1, 'default', null, false);
    send(events);
    let block = document.querySelector('.html-render');
    let frame = block?.querySelector('iframe');
    check('visual is a direct timeline child', block?.parentElement.id === 'messages');
    check('work fold leaves visual out', !!document.querySelector('.turn-fold') && !block?.closest('.turn-fold'));
    check('visual precedes final reply', !!(block?.compareDocumentPosition(document.querySelector('#messages > .assistant')) & Node.DOCUMENT_POSITION_FOLLOWING));
    check('srcdoc has only allow-scripts sandbox', frame?.getAttribute('sandbox') === 'allow-scripts' && !frame.srcdoc.includes('allow-same-origin'));
    for (const directive of ["default-src 'none'", "connect-src 'none'", "form-action 'none'", "base-uri 'none'", "frame-src 'none'"]) check('CSP ' + directive, frame?.srcdoc.includes(directive));
    check('iframe height is fitted', frame?.style.height === '240px');
    send([done('visual')]);
    check('duplicate completion does not duplicate render', document.querySelectorAll('.html-render iframe').length === 1);
    block.querySelector('button').click();
    check('expand stays inside chat', block.classList.contains('expanded'));
    block.dispatchEvent(new KeyboardEvent('keydown', { key: 'Escape', bubbles: true }));
    check('Escape collapses view', !block.classList.contains('expanded'));
    window.__setAppearance('light');
    window.__resetConversation();
    check('reset removes iframe', document.querySelectorAll('.html-render iframe').length === 0);
    window.__replay(events);
    check('replay restores exactly one visual outside fold', document.querySelectorAll('.html-render iframe').length === 1 && !document.querySelector('.html-render').closest('.turn-fold'));
    for (const name of ['html_render','mcp_gitterm-tasks_html_render','gitterm_tasks_html_render']) {
      window.__resetConversation();send([prompt,start('visual',name,input),done('visual')]);
      check('publishes exact tool name '+name,document.querySelectorAll('.html-render iframe').length===1);
    }
    window.__resetConversation();window.__replay(events);
    window.__appendEvent(start('bad', 'html_render', input)); window.__appendEvent(done('bad', true));
    check('failed tool does not publish', document.querySelectorAll('.html-render iframe').length === 1);
    window.__appendEvent(start('unrelated','Read',input));window.__appendEvent(H('item_completed',{id:'unrelated',output:JSON.stringify({html_render:input}),is_error:false}));
    check('unrelated tool output cannot publish HTML',document.querySelectorAll('.html-render iframe').length===1);
    check('byte and height limits in UI', !GitTermHtml.valid('é'.repeat(262145), 240) && !GitTermHtml.valid('ok', 99) && !GitTermHtml.valid('ok', 4001));
    check('safe URLs reject scripts/files/relative/credentials', ['javascript:alert(1)', 'file:///tmp/x', '//example.com', 'https://u:p@example.com'].every((url) => GitTermHtml.httpUrl(url) === null));
    check('safe URL accepts http(s)', GitTermHtml.httpUrl('https://example.com/path') === 'https://example.com/path');

    const nested = {};
    window.addEventListener('message', (e) => { if (e.data?.probe) nested[e.data.probe] = e.data.value; });
    const payload = `<body><a id="link" style="display:block;height:40px" href="https://example.com/visual">Open example</a><script>
      const report = (probe, value) => parent.postMessage({probe, value}, '*');
      for (const [key, fn] of Object.entries({parent:()=>parent.document.body, storage:()=>localStorage.getItem('secret'), ipc:()=>parent.ipc.postMessage('attack')})) {
        try { fn(); report(key,false); } catch(e) { report(key,true); }
      }
      try { report('cookies', document.cookie === ''); } catch(e) { report('cookies', true); }
      report('own-ipc', typeof window.ipc === 'undefined' && typeof window.__gittermNativePost === 'undefined');
      report('rtc', typeof RTCPeerConnection === 'undefined' && typeof webkitRTCPeerConnection === 'undefined');
      try { window.webkit.messageHandlers.ipc.postMessage(JSON.stringify({type:'submit',tabId:99,text:'attack'})); } catch(e) {}
      try { window.webkit.messageHandlers.ipc.postMessage(JSON.stringify({token:'wrong',message:'attack'})); } catch(e) {}
      parent.postMessage({type:'open_url',url:'https://example.com/forged'}, '*');
      document.getElementById('link').click();
      const c = new MessageChannel(); parent.postMessage({type:'gitterm-html-ready'}, '*', [c.port2]); c.port1.postMessage({type:'link',url:'https://example.com/forged-channel'});
      const childCode = "parent.parent.postMessage({probe:'nested-srcdoc',value:typeof RTCPeerConnection === 'undefined' && typeof webkitRTCPeerConnection === 'undefined'}, '*')";
      const f = document.createElement('iframe'); f.srcdoc = '<script>'+childCode+'<\\/script>'; document.body.append(f);
      const b = document.createElement('iframe'); document.body.append(b); try { report('blank', typeof b.contentWindow.RTCPeerConnection === 'undefined'); } catch(e) { report('blank', true); }
      for (const [kind,src] of [['data', 'data:text/html,<script>parent.parent.postMessage({probe:"nested-data",value:false},"*")<\\/script>'], ['javascript', 'javascript:parent.parent.postMessage({probe:"nested-javascript",value:typeof RTCPeerConnection === "undefined"},"*")']]) {
        const f=document.createElement('iframe'); f.src=src; document.body.append(f);
      }
      document.body.insertAdjacentHTML('beforeend','<img src="http://127.0.0.1:1/leak"><form action="http://127.0.0.1:1/form"><input name="secret" value="x"></form>');
      try { document.querySelector('form').submit(); } catch(e) {}
      fetch('http://127.0.0.1:1/fetch').catch(()=>{}); try { new WebSocket('ws://127.0.0.1:1/socket'); } catch(e) {}
      try { report('popup', window.open('https://example.com/popup') === null); } catch(e) { report('popup', true); }
      try { parent.document.cookie = 'attack=1'; report('write-parent', false); } catch(e) { report('write-parent',true); }
      console.log('preview-console-ok');
    <\/script></body>`;
    const opened = [];
    const probe = GitTermHtml.create(payload, 300, 'dark', (url) => { opened.push(url); window.__gittermNativePost(JSON.stringify({type:'open_url',url})); });
    probe.frame.style.cssText = 'position:fixed;left:20px;top:100px;width:500px;height:300px;z-index:10000;border:0';
    document.body.append(probe.frame);
    await wait(1200);
    for (const key of ['parent','cookies','storage','ipc','own-ipc','rtc','blank','popup','write-parent']) check('sandbox blocks ' + key, nested[key] === true);
    check('nested srcdoc RTC guard', nested['nested-srcdoc'] === undefined || nested['nested-srcdoc'] === true);
    check('nested data blocked', nested['nested-data'] === undefined);
    check('nested javascript RTC guard', nested['nested-javascript'] === undefined || nested['nested-javascript'] === true);
    check('forged messages and synthetic clicks do not open links', opened.length === 0);
    check('console is captured', probe.state.consoleMessages.some((m) => m.text.includes('preview-console-ok')));
    check('content height arrives', probe.state.ready && probe.state.contentHeight >= 100);
    probe.frame.remove(); document.body.append(probe.frame); await wait(400);
    check('reinserted iframe does not accept synthetic links', opened.length === 0);
    probe.setTheme('light'); await wait(100);
    // Read theme inside the sandbox via the private console bridge in a
    // separate page: the script itself remains unable to inspect the host.
    const themed = GitTermHtml.create('<script>setTimeout(()=>console.log(getComputedStyle(document.documentElement).getPropertyValue("--bg")),200)<\/script>', 100, 'light');
    document.body.append(themed.frame); await wait(400);
    check('light theme variables arrive', themed.state.consoleMessages.some((m) => m.text.includes('#eff1f5')));
    themed.dispose();
    window.__linkProbe = { probe, opened };
    if (window.__testMode === 'chrome') {
      window.__awaitTrustedClick = true;
      while (opened.length === 0 && !window.__skipClick) await wait(50);
      check('trusted link opens through safe opener', opened[0] === 'https://example.com/visual');
    }
    probe.dispose();
    // Hostile document.write cannot create a fresh RTC-capable realm.
    const written = GitTermHtml.create('<script>document.write(\'<iframe srcdoc="&lt;script&gt;parent.parent.postMessage({probe: &quot;written&quot;, value: typeof RTCPeerConnection === &quot;undefined&quot;}, &quot;*&quot;)&lt;/script&gt;"></iframe>\')<\/script>',100,'dark');
    document.body.append(written.frame); await wait(250);
    check('document.write descendant RTC guard', nested.written === undefined || nested.written === true); written.dispose();
    if (window.__testMode === 'webkit') {
      // A CSP meta cannot block self-navigation; the native navigation
      // policy must stop both script navigation and meta refresh.
      for (const html of [
        '<script>setTimeout(()=>location.href="http://127.0.0.1:1/navigation",50)<\/script>',
        '<meta http-equiv="refresh" content="0;url=http://127.0.0.1:1/refresh">'
      ]) {
        const nav = GitTermHtml.create(html, 100, 'dark'); document.body.append(nav.frame);
        await wait(250); nav.dispose();
      }
    }
  } catch (error) { check('harness exception: ' + error.stack, false); }
  const result = { checks, failures: checks.filter((c) => !c.ok) };
  window.__testResult = result;
  if (window.__testMode === 'webkit') window.__gittermNativePost(JSON.stringify({ type: 'test_report', result }));
})();
