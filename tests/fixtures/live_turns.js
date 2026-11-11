// Real streaming events against the chat page, without an agent process.
(async () => {
  const results = [];
  const check = (name, ok) => results.push((ok ? 'PASS ' : 'FAIL ') + name);
  const H = (type, data) => ({ kind: 'harness', event: { type, data } });
  const send = (...events) => events.forEach((event) => {
    window.__appendEvent(event);
    const error = [...document.querySelectorAll('.error-block')].find((n) => n.textContent.startsWith('Chat view could not render'));
    if (error) throw new Error(error.textContent);
  });
  const prompt = { type: 'user_prompt', text: 'Check the project' };
  const start = (id, name = 'Bash', input = {}) => H('item_started', { id, kind: { name, input } });
  const done = (id, is_error = false) => H('item_completed', { id, output: 'result', is_error });
  const end = H('turn_completed', { status: { state: 'completed' }, usage: {} });
  const card = (id) => document.querySelector('[data-tool-id="' + id + '"]');
  const runs = () => [...document.querySelectorAll('.live-tool-run')];
  const label = (run) => run?.querySelector(':scope > summary .tr-label')?.textContent;
  const reset = () => { window.__resetConversation(); send(prompt); };
  // Drive paint callbacks explicitly: dump-dom's virtual clock does not
  // reliably paint between timers. Toggle events still run on the real DOM.
  const requestFrame = window.requestAnimationFrame;
  const frames = [];
  window.requestAnimationFrame = (callback) => frames.push(callback);
  const settle = async () => {
    await new Promise((resolve) => setTimeout(resolve, 0));
    while (frames.length) frames.shift()(performance.now());
    await new Promise((resolve) => setTimeout(resolve, 0));
  };
  const messages = document.getElementById('messages');
  const jump = document.getElementById('jump');
  window.ipc = { postMessage: () => {} };
  window.__setTabId(1, 'default', null, false);
  try {
    reset();
    send(H('thinking_delta', ''));
    check('redacted thinking stays activity-only', !document.querySelector('details.thinking'));
    send(H('thinking_delta', 'Planning '), H('thinking_delta', 'the checks'));
    const thinking = document.querySelector('details.thinking');
    check('streaming thinking is open with accumulated text', thinking?.open && thinking.querySelector('.body').textContent === 'Planning the checks');
    send(start('one', 'Bash', { command: 'cargo fmt -- --check' }));
    const firstRun = runs()[0];
    check('live summary starts closed', firstRun && !firstRun.open);
    firstRun.open = true;
    send(done('one'), start('two', 'Bash', { command: 'cargo clippy' }), done('two'), start('three'));
    send(H('item_input_delta', { id: 'three', partial_json: '{"command":' }),
      H('item_input_delta', { id: 'three', partial_json: '"cargo test"}' }));
    check('three calls share one row updated in place', runs().length === 1 && runs()[0] === firstRun && firstRun.open && firstRun.querySelectorAll('details.tool').length === 3);
    check('live counts and streamed current command use shared wording', label(firstRun) === 'Ran 3 commands · now: cargo test');
    check('thinking remains open outside the tool run', thinking.open && !thinking.closest('.tool-run'));
    firstRun.querySelector('summary').click();
    check('summary can close and reopen the cards', !firstRun.open);
    firstRun.querySelector('summary').click();
    check('opening summary exposes its cards', firstRun.open && card('three').getClientRects().length > 0);
    send(done('three'));
    check('completed calls remove the current action suffix', label(firstRun) === 'Ran 3 commands');
    send(H('text_delta', 'Checking files.'), start('read-one', 'Read', { file_path: 'Cargo.toml' }), done('read-one'),
      start('read-two', 'Read', { file_path: 'Cargo.toml' }), done('read-two'));
    check('text splits runs and repeated reads count files once', runs().length === 2 && label(runs()[1]) === 'Read 1 file');
    send(H('thinking_delta', 'One more check'), start('four', 'Bash', { command: 'cargo test' }), done('four'));
    check('new thinking splits runs and all thinking stays open', runs().length === 3 && [...document.querySelectorAll('details.thinking')].every((n) => n.open && !n.closest('.tool-run')));
    send({ kind: 'turn_timing', start_ms: 1000, end_ms: 6000 }, H('text_delta', 'Finished.'), end);
    const fold = document.querySelector('.turn-fold');
    check('turn end uses closed existing duration fold with final answer outside', !runs().length && fold && !fold.open && fold.querySelector('.tf-label').textContent === 'Worked for 5.0s' && document.querySelector('#messages > .assistant')?.textContent.trim() === 'Finished.');
    check('finished runs retain existing thinking grouping with no live suffix or nested runs', [...fold.querySelectorAll('.tool-run')].length === 2 && !fold.querySelector('.tool-run .tool-run') && !fold.textContent.includes('now:'));

    reset();
    send(start('before'), done('before'), start('permission'), start('after'));
    send(H('runtime_request', { request_id: 'approve', tool_use_id: 'permission', kind: { type: 'permission', tool_name: 'Bash', input: { command: 'cargo test' } } }));
    check('pending permission card leaves summary in original position', !card('permission').closest('.tool-run') && card('permission').parentElement.id === 'messages' && runs().length === 2 && runs()[0].nextElementSibling === card('permission') && card('permission').nextElementSibling === runs()[1]);
    send(H('runtime_request_resolved', { request_id: 'approve' }));
    check('resolved permission rejoins its consecutive run', runs().length === 1 && runs()[0].querySelectorAll('details.tool').length === 3);
    send(done('permission', true));
    check('failed tool leaves summary in place', card('permission').matches('.error') && !card('permission').closest('.tool-run') && runs().length === 2);
    send(end);
    check('finished fold still leaves failed card visible', !card('permission').closest('.turn-fold, .tool-run'));

    reset();
    send(start('safe'), done('safe'), start('waiting'), H('runtime_request', { request_id: 'wait', tool_use_id: 'waiting', kind: { type: 'permission', tool_name: 'Bash', input: {} } }), end);
    check('pending request stays outside finished fold too', !card('waiting').closest('.turn-fold, .tool-run') && !!document.querySelector('.turn-fold'));

    reset();
    send(start('agent', 'Agent', { description: 'Check a file' }), start('regular'));
    check('running subagent stays outside live run', !card('agent').closest('.tool-run') && runs().length === 1);
    send(done('agent'));
    check('finished subagent may join ordinary tool run', !!card('agent').closest('.live-tool-run'));
    send(H('subagent_event', { parent_tool_use_id: 'agent', event: { type: 'turn_started', data: {} } }));
    check('subagent restarting leaves summary again', !card('agent').closest('.tool-run'));
    send(H('subagent_event', { parent_tool_use_id: 'agent', event: { type: 'item_started', data: { id: 'nested', kind: { name: 'Read', input: {} } } } }),
      H('runtime_request', { request_id: 'nested-approve', tool_use_id: 'nested', kind: { type: 'permission', tool_name: 'Read', input: {} } }));
    check('attention panel remains visible', document.querySelector('#request-panel.visible'));

    reset();
    send(start('early'), done('early'), H('error', 'A visible error'), start('late'));
    check('error block stays outside separate live runs', runs().length === 2 && document.querySelector('.error-block')?.parentElement.id === 'messages');
    send({ type: 'user_prompt', text: 'Follow-up', id: 'queued', queued: true }, start('queued-tool'));
    check('queued prompt splits live runs', runs().length === 3 && document.querySelector('[data-msg-id="queued"]')?.parentElement.id === 'messages');

    reset();
    send(start('before-warning'), H('remote_disconnected', { machine: 'test-host', error: 'connection lost' }), start('after-warning'));
    check('warning stays visible between separate runs', document.querySelector('.note.warn')?.parentElement.id === 'messages' && runs().length === 2);
    send({ kind: 'delegation', delegation: {
      delegation_id: 'live-delegation', kind: 'consult', brief: 'Check the implementation',
      child: { runner: 'codex_consult', model: 'test-model' }, status: { state: 'running' },
      created_at: '2026-11-01T10:00:00Z', updated_at: '2026-11-01T10:00:00Z',
    } }, start('after-delegation'));
    check('delegation card stays visible between separate runs', document.querySelector('.dlg')?.parentElement.id === 'messages' && runs().length === 3);

    const history = [prompt, H('thinking_delta', 'Historical thinking'), start('history'), done('history'), H('text_delta', 'Done.'), { kind: 'turn_timing', start_ms: 1000, end_ms: 3000 }, end];
    window.__resetConversation(); window.__replay(history.slice(0, 4));
    check('running history replay keeps original cards and thinking closed', !runs().length && !document.querySelector('details.thinking').open && card('history').parentElement.id === 'messages');
    window.__resetConversation(); window.__replay(history);
    check('completed replay keeps original fold and summary wording', !runs().length && !document.querySelector('.turn-fold').open && document.querySelector('.tf-label').textContent === 'Worked for 2.0s' && document.querySelector('.tr-label').textContent === 'Ran 1 command');

    // F1: replay can leave one ongoing turn across the history window boundary.
    const longHistory = [prompt];
    for (let i = 0; i < 250; i++) longHistory.push(start('replay-' + i), done('replay-' + i));
    window.__resetConversation(); window.__replay(longHistory);
    const visibleTools = [...messages.querySelectorAll('details.tool')];
    send(start('after-replay'));
    check('F1 live grouping preserves every visible replay card', runs().length === 1 && visibleTools.every((n) => n.isConnected && n.closest('.live-tool-run') === runs()[0]) && !!card('after-replay')?.isConnected);
    document.querySelector('.earlier').click();
    document.querySelector('.earlier')?.click();
    check('F1 detached history stays ungrouped until restored', messages.querySelectorAll('details.tool').length === 251 && card('replay-0')?.parentElement === messages);

    // F2: an automatic reveal must not be mistaken for a summary click.
    reset();
    send(H('text_delta', 'Earlier output.\n'.repeat(100)), start('resume-agent', 'Agent', { description: 'Check a file' }), done('resume-agent'), start('resume-tool'));
    await settle();
    send(H('subagent_event', { parent_tool_use_id: 'resume-agent', event: { type: 'turn_started', data: {} } }));
    send(H('text_delta', 'More output.\n'.repeat(100)));
    await settle();
    check('F2 automatic subagent reveal preserves follow mode', !jump.classList.contains('visible'));
    check('F2 later output still follows the bottom', messages.scrollHeight - messages.scrollTop - messages.clientHeight < 40);
    runs()[0].open = true;
    send(H('text_delta', 'Continuing output.\n'.repeat(100)));
    await settle();
    check('F2 direct programmatic open also preserves follow', !jump.classList.contains('visible') && messages.scrollHeight - messages.scrollTop - messages.clientHeight < 40);

    // F3: the wrapper disappears at turn end, but its reader anchor must survive.
    reset();
    for (let i = 0; i < 30; i++) send(start('anchor-' + i), done('anchor-' + i));
    send(H('text_delta', 'Final answer.\n'.repeat(100)));
    await settle();
    runs()[0].querySelector('summary').click();
    await settle();
    check('reader opening a live summary still pauses follow', jump.classList.contains('visible'));
    messages.scrollTop = runs()[0].offsetTop + 300;
    messages.dispatchEvent(new WheelEvent('wheel', { deltaY: -100 }));
    messages.dispatchEvent(new Event('scroll'));
    check('F3 reader is inside expanded live run', jump.classList.contains('visible') && messages.scrollTop > runs()[0].offsetTop && messages.scrollTop < runs()[0].offsetTop + runs()[0].offsetHeight);
    send(end);
    check('F3 turn end carries reader anchor to the finished fold', Math.abs(messages.scrollTop - document.querySelector('.turn-fold').offsetTop) < 2);
    // F1 round 2: trimming existing live wrappers must not change chunk sizes.
    reset();
    for (let i = 0; i < 150; i++) {
      send(start('trim-' + i + '-a'), done('trim-' + i + '-a'),
        start('trim-' + i + '-b'), done('trim-' + i + '-b'), H('text_delta', 'Checking group ' + i + '.'));
    }
    const beforeTrim = runs();
    window.__replay([]); // Trim the existing live DOM, without replacing it.
    const detachedRuns = beforeTrim.filter((run) => !run.isConnected);
    const detachedMarkup = detachedRuns.map((run) => run.outerHTML);
    send(H('text_delta', ' Still checking.'));
    check('F1 detached live wrappers remain unchanged after another event', detachedRuns.length > 0 && detachedRuns.every((run, i) => run.parentNode?.nodeType === Node.DOCUMENT_FRAGMENT_NODE && run.outerHTML === detachedMarkup[i]));
    document.querySelector('.earlier').click();
    check('F1 first restored chunk leaves 100 blocks hidden', document.querySelector('.earlier')?.textContent === 'Show earlier · 100 blocks hidden');
    document.querySelector('.earlier')?.click();
    check('F1 second restored chunk leaves 50 blocks hidden', document.querySelector('.earlier')?.textContent === 'Show earlier · 50 blocks hidden');
    const restoredRun = detachedRuns.find((run) => run.isConnected);
    send(H('text_delta', ' Finished checking.'));
    check('F1 restored live wrapper is reused on subsequent events', restoredRun?.isConnected && runs().includes(restoredRun) && restoredRun.querySelectorAll('details.tool').length === 2);
    send(end);
    check('F1 turn end folds restored and detached runs without losing cards', !document.querySelector('.earlier') && !runs().length && messages.querySelectorAll('.turn-fold details.tool').length === 300 && !messages.querySelector('.tool-run .tool-run') && [...document.querySelectorAll('#messages > .assistant')].some((n) => n.textContent.includes('Finished checking.')));

    // TRU-270: exercise the actual action/shortcut, never a separate serializer.
    const markdownIpc = [];
    window.ipc = { postMessage: (body) => markdownIpc.push(JSON.parse(body)) };
    const selection = window.getSelection();
    const fixture = (html) => {
      window.__resetConversation();
      messages.innerHTML = html;
      document.activeElement.blur();
      selection.removeAllRanges();
      markdownIpc.length = 0;
    };
    const leaf = (selector) => document.querySelector(selector).firstChild;
    const select = (start, from, end = start, to = end.length) => {
      const range = document.createRange(); range.setStart(start, from); range.setEnd(end, to);
      selection.removeAllRanges(); selection.addRange(range);
      return range;
    };
    const copied = () => {
      markdownIpc.length = 0;
      window.__copySelectionMarkdown(false);
      return markdownIpc.find((m) => m.type === 'copy_markdown')?.text;
    };
    const equalCopy = (name, expected) => {
      const actual = copied();
      check('markdown ' + name + (actual === expected ? '' : ' got ' + JSON.stringify(actual)), actual === expected);
    };
    fixture('<div class="assistant"><p>Before <strong id="bold">bold text</strong> after</p></div>');
    select(leaf('#bold'), 1, leaf('#bold'), 7); equalCopy('partial bold', '**old te**');
    fixture('<div class="assistant"><p><strong id="nested">bold <em id="italic">italic</em> end</strong></p></div>');
    select(leaf('#nested'), 2, leaf('#italic'), 3); equalCopy('nested formatting clipped both ends', '**ld *ita***');
    fixture('<div class="assistant"><p><a id="link" href="https://example.org/a(b)">Unicode 😀 café</a></p></div>');
    select(leaf('#link'), 8, leaf('#link'), 15); equalCopy('partial link Unicode', '[😀 café](https://example.org/a%28b%29)');
    selection.setBaseAndExtent(leaf('#link'), 15, leaf('#link'), 8); equalCopy('backward selection', '[😀 café](https://example.org/a%28b%29)');
    fixture('<div class="assistant"><p><code id="inline">a`b``c</code></p></div>');
    select(leaf('#inline'), 1, leaf('#inline'), 5); equalCopy('partial code safe delimiter', '``` `b`` ```');
    fixture('<div class="assistant"><pre><code class="language-rust" id="fence">first\n```\n\n\nlast</code></pre></div>');
    select(leaf('#fence'), 3); equalCopy('safe fence language and literal blank lines', '````rust\nst\n```\n\n\nlast\n````');
    fixture('<div class="assistant"><p id="before-fold">before</p><details class="tool"><summary>visible summary</summary><div>hidden tool payload</div></details><p id="after-fold">after</p></div>');
    select(leaf('#before-fold'), 0, leaf('#after-fold'));
    const closedFoldCopy = copied();
    check('markdown excludes content in a closed details element', !closedFoldCopy.includes('hidden tool payload') && closedFoldCopy.includes('visible summary') && closedFoldCopy.includes('before') && closedFoldCopy.includes('after'));
    fixture('<details class="turn-fold" open><summary>worked</summary><div class="tf-body"><div class="assistant"><p><strong id="fold-bold">bold in fold</strong></p></div></div></details>');
    select(leaf('#fold-bold'), 0); equalCopy('expanded turn fold keeps assistant formatting', '**bold in fold**');
    fixture('<div class="assistant"><h2 id="heading">Heading</h2><blockquote><p><s id="strike">deleted</s><br><em id="break">line</em></p></blockquote></div>');
    select(leaf('#heading'), 2, leaf('#break'), 2); equalCopy('heading quote strike and hard break', '## ading\n\n> ~~deleted~~  \n> *li*');
    fixture('<div class="assistant"><ul><li>omit</li><li id="outer">parent<ul><li id="inner">nested</li><li>omit too</li></ul></li></ul></div>');
    select(leaf('#outer'), 2, leaf('#inner'), 4); equalCopy('nested selected list items', '- rent\n  - nest');
    select(leaf('#inner'), 0, leaf('#inner'), 4); equalCopy('nested item alone omits unselected parent', '  - nest');
    fixture('<div class="assistant"><ol start="4"><li>omit</li><li id="ordered">second</li><li>omit</li></ol></div>');
    select(leaf('#ordered'), 1, leaf('#ordered'), 4); equalCopy('ordered selected item', '5. eco');
    reset(); send(H('text_delta', '```rust\nlet value = true;\n```'), end);
    document.activeElement.blur();
    const renderedCode = document.querySelector('.assistant .codeblock code');
    const codeRange = document.createRange(); codeRange.selectNodeContents(renderedCode);
    selection.removeAllRanges(); selection.addRange(codeRange);
    equalCopy('real renderer preserves code language without UI label', '```rust\nlet value = true;\n```');
    const table = '<div class="assistant"><table><thead><tr><th id="h1">Name</th><th id="h2">Value</th></tr></thead><tbody><tr><td id="c1">alpha</td><td id="c2">beta</td></tr><tr><td id="c3">gamma</td><td id="c4">delta</td></tr></tbody></table></div>';
    fixture(table); select(leaf('#h1'), 0, leaf('#c4'), 5);
    equalCopy('complete table with header', '| Name | Value |\n| --- | --- |\n| alpha | beta |\n| gamma | delta |');
    fixture('<div class="assistant"><table><thead><tr><th>Value</th></tr></thead><tbody><tr><td><code id="pipe-code">a|b</code></td></tr></tbody></table></div>');
    const pipeTable = document.querySelector('table');
    const pipeRange = document.createRange(); pipeRange.selectNodeContents(pipeTable);
    selection.removeAllRanges(); selection.addRange(pipeRange);
    equalCopy('table inline code escapes a pipe', '| Value |\n| --- |\n| `a\\|b` |');
    fixture(table);
    select(leaf('#c1'), 2, leaf('#c4'), 3); equalCopy('partial table TSV', 'pha\tbeta\ngamma\tdel');
    select(leaf('#h1'), 1, leaf('#h2'), 3); equalCopy('partial table header TSV', 'ame\tVal');
    fixture('<div class="assistant"><p id="reply1">first <b>bold</b></p></div><div class="user-msg" id="user">literal **user**</div><details class="tool" open><summary>tool label<button>no button</button></summary><div id="tool">visible <b>tool</b><span hidden>secret</span><span style="display:none">payload</span></div></details><div class="assistant"><p id="reply2">last reply</p></div>');
    select(leaf('#reply1'), 2, leaf('#reply2'), 4);
    equalCopy('several replies with literal user and visible tool text', 'rst **bold**\n\nliteral **user**\n\ntool label\n\nvisible tool\n\nlast');
    fixture('<div class="user-msg" id="literal-user">\n\nliteral **user**\n\n</div>');
    select(leaf('#literal-user'), 0); equalCopy('user message retains literal boundary newlines', '\n\nliteral **user**\n\n');
    fixture('<div class="assistant"><p id="reply1">first <b>bold</b></p></div><div class="user-msg" id="user">literal **user**</div><div class="assistant"><p id="reply2">last reply</p></div>');
    // Native context menus remain unmodified by the page over every target.
    const promptInput = document.getElementById('prompt');
    const originalExec = document.execCommand.bind(document);
    promptInput.focus(); promptInput.value = 'draft'; promptInput.setSelectionRange(5, 5);
    originalExec('insertText', false, ' edit');
    promptInput.blur();
    const selected = select(leaf('#reply1'), 1, leaf('#reply2'), 3);
    const focusBefore = document.activeElement;
    const unchanged = () => document.activeElement === focusBefore && selection.rangeCount === 1
      && selection.getRangeAt(0).startContainer === selected.startContainer && selection.getRangeAt(0).startOffset === selected.startOffset
      && selection.getRangeAt(0).endContainer === selected.endContainer && selection.getRangeAt(0).endOffset === selected.endOffset;
    for (const target of [document.querySelector('#reply1'), document.querySelector('#reply1 b'), document.querySelector('#user'), promptInput]) {
      const markup = document.body.innerHTML;
      const event = new MouseEvent('contextmenu', { bubbles: true, cancelable: true });
      target.dispatchEvent(event);
      check('native contextmenu kept over ' + target.tagName, !event.defaultPrevented && unchanged() && document.body.innerHTML === markup);
      check('menu eligibility follows transcript target ' + target.tagName, window.__prepareCopyMarkdownMenu() === (target !== promptInput));
    }
    document.querySelector('#reply1').dispatchEvent(new MouseEvent('contextmenu', { bubbles: true, cancelable: true }));
    markdownIpc.length = 0; window.__copySelectionMarkdown(true);
    check('menu action keeps focus range draft', markdownIpc.length === 1 && unchanged() && promptInput.value === 'draft edit');
    let copyCommands = 0;
    document.execCommand = (command, ...args) => command === 'copy' ? (++copyCommands, true) : originalExec(command, ...args);
    markdownIpc.length = 0;
    document.dispatchEvent(new KeyboardEvent('keydown', { key: 'c', metaKey: true, bubbles: true, cancelable: true }));
    check('Cmd+C existing copy exactly once and no markdown IPC', copyCommands === 1 && markdownIpc.length === 0);
    document.dispatchEvent(new KeyboardEvent('keydown', { key: 'C', metaKey: true, shiftKey: true, bubbles: true, cancelable: true }));
    check('Cmd+Shift+C only selected text and no execCommand', markdownIpc.length === 1 && markdownIpc[0].type === 'copy_markdown' && copyCommands === 1 && unchanged());
    promptInput.focus(); originalExec('undo');
    check('markdown leaves composer undo history intact', promptInput.value === 'draft');
    const findInput = document.body.appendChild(document.createElement('input')); findInput.id = 'test-find';
    const richInput = document.body.appendChild(document.createElement('div')); richInput.contentEditable = 'true'; richInput.id = 'test-editable';
    for (const input of [promptInput, findInput, richInput]) {
      input.focus(); markdownIpc.length = 0;
      input.dispatchEvent(new KeyboardEvent('keydown', { key: 'C', metaKey: true, shiftKey: true, bubbles: true, cancelable: true }));
      check('markdown shortcut ignored in ' + input.id, markdownIpc.length === 0);
    }
    findInput.remove(); richInput.remove();
    document.execCommand = originalExec;
    fixture('<div class="assistant"><p><code id="code-menu">code</code> <a id="link-menu" href="https://example.org">link</a></p><span class="tail" id="tail">streaming tail</span></div>');
    for (const selector of ['#code-menu', '#link-menu']) {
      select(leaf(selector), 0);
      const event = new MouseEvent('contextmenu', { bubbles: true, cancelable: true });
      const target = document.querySelector(selector); target.dispatchEvent(event);
      check('code/link native menu not cancelled ' + selector, !event.defaultPrevented && window.__prepareCopyMarkdownMenu());
    }
    select(leaf('#tail'), 2, leaf('#tail'), 9);
    document.querySelector('#tail').dispatchEvent(new MouseEvent('contextmenu', { bubbles: true, cancelable: true }));
    leaf('#tail').appendData(' changed after opening');
    markdownIpc.length = 0; window.__copySelectionMarkdown(true);
    check('streaming snapshot at menu open', markdownIpc[0]?.text === 'reaming');
    select(leaf('#tail'), 0, leaf('#tail'), 9); equalCopy('streaming shortcut snapshot', 'streaming');
    selection.removeAllRanges(); equalCopy('empty selection copies nothing', undefined);
    markdownIpc.length = 0;
    document.dispatchEvent(new KeyboardEvent('keydown', { key: 'C', metaKey: true, shiftKey: true, bubbles: true, cancelable: true }));
    check('empty shortcut has no whole-reply fallback', markdownIpc.length === 0);
    window.__resetConversation();

    check('no event-render errors', ![...document.querySelectorAll('.error-block')].some((n) => n.textContent.startsWith('Chat view could not render')));
  } catch (error) {
    results.push('FAIL ' + error.stack);
  }
  window.requestAnimationFrame = requestFrame;
  const result = document.createElement('pre');
  result.id = 'live-turn-test-result';
  result.textContent = results.join('\n');
  document.body.appendChild(result);
})();
