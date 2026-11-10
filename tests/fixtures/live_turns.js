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
    // A wide fixture exercises all caps even on a small physical screen.
    const originalWidth = document.body.style.width;
    document.body.style.width = '1600px';
    const column = () => {
      const style = getComputedStyle(messages);
      return messages.clientWidth - parseFloat(style.paddingLeft) - parseFloat(style.paddingRight);
    };
    const footer = document.getElementById('footer');
    const rem = parseFloat(getComputedStyle(document.documentElement).fontSize);
    check('chat width defaults to Medium', Math.abs(column() - 60 * rem) < 1);
    check('chat text defaults to 15px with a fixed 14px rem', getComputedStyle(document.body).fontSize === '15px' && rem === 14);
    reset();
    send(H('text_delta', 'A message with `inline code`.\n\n```js\nconst size = 15;\n```'), start('font-size', 'Bash', { command: 'cargo test' }));
    card('font-size').open = true;
    send({ kind: 'delegation', delegation: {
      delegation_id: 'font-delegation', kind: 'consult', brief: 'Check text scaling',
      child: { runner: 'codex_consult', model: 'test-model' }, status: { state: 'running' },
      created_at: '2026-11-01T10:00:00Z', updated_at: '2026-11-01T10:00:00Z',
    } });
    const textNodes = ['.dlg-kind', '.ro-row', '.assistant p', '.assistant p code', '.codeblock pre code', '.live-tool-run > summary', '.tool-name', '.tool-brief', '#prompt', '#submit', '#mode'];
    const sizes = () => textNodes.map((selector) => {
      const node = document.querySelector(selector);
      if (!node) throw new Error('Missing text-size fixture: ' + selector);
      return parseFloat(getComputedStyle(node).fontSize);
    });
    window.__setChatFontSize(14);
    const oldSizes = sizes();
    const oldColumn = column();
    const oldFooter = footer.getBoundingClientRect().width;
    for (const size of [11, 15, 22]) {
      window.__setChatFontSize(size);
      const scaled = sizes();
      check('all sampled text scales proportionally at ' + size + 'px', scaled.every((value, i) => Math.abs(value / oldSizes[i] - size / 14) < 0.001));
      check('column and composer width stay fixed at ' + size + 'px', Math.abs(column() - oldColumn) < 1 && footer.getBoundingClientRect().width === oldFooter && getComputedStyle(document.documentElement).fontSize === '14px');
    }
    window.__setChatFontSize(1);
    check('page clamps text size to 11px', getComputedStyle(document.body).fontSize === '11px');
    window.__setChatFontSize(99);
    check('page clamps text size to 22px', getComputedStyle(document.body).fontSize === '22px');
    {
      // Resizing keeps the reader's place: at the bottom when following,
      // the same block at the same height when scrolled back.
      window.__setChatFontSize(14);
      const fillers = [];
      for (let i = 0; i < 60; i++) {
        const div = document.createElement('div');
        div.className = 'assistant';
        div.textContent = 'Filler paragraph ' + i + ' with enough words to wrap at larger text sizes in the column.';
        messages.appendChild(div);
        fillers.push(div);
      }
      await settle();
      messages.scrollTop = messages.scrollHeight;
      messages.dispatchEvent(new Event('scroll'));
      await settle();
      window.__setChatFontSize(22);
      await settle();
      check('following view stays at the bottom after a text-size change', messages.scrollHeight - messages.scrollTop - messages.clientHeight < 2);
      messages.dispatchEvent(new WheelEvent('wheel', { deltaY: -400 }));
      messages.scrollTop = Math.floor(messages.scrollHeight / 3);
      messages.dispatchEvent(new Event('scroll'));
      await settle();
      const top = messages.scrollTop;
      const block = [...messages.children].find((c) => c.offsetTop + c.offsetHeight > top);
      const before = block.offsetTop - messages.scrollTop;
      window.__setChatFontSize(15);
      await settle();
      check('scrolled-back view keeps its block in place after a text-size change', Math.abs(block.offsetTop - messages.scrollTop - before) < 2);
      for (const div of fillers) div.remove();
    }
    window.__setChatFontSize(15);
    const posted = [];
    window.ipc = { postMessage: (message) => posted.push(JSON.parse(message)) };
    for (const [key, shiftKey] of [['=', false], ['-', false], ['+', true], ['_', true]]) {
      const event = new KeyboardEvent('keydown', { key, metaKey: true, shiftKey, bubbles: true, cancelable: true });
      document.getElementById('prompt').dispatchEvent(event);
      check('focused composer forwards text-size key ' + key, event.defaultPrevented && posted.at(-1)?.type === 'hostkey' && posted.at(-1)?.key === key && posted.at(-1)?.shift === shiftKey);
    }
    window.ipc = { postMessage: () => {} };

    for (const [preset, size] of [['narrow', 46], ['medium', 60], ['wide', 72]]) {
      window.__setChatWidth(preset);
      check(preset + ' changes the column immediately', Math.abs(column() - size * rem) < 1);
      const panel = footer.getBoundingClientRect();
      const timeline = messages.getBoundingClientRect();
      check(preset + ' input panel matches the column', Math.abs(panel.width - column() - 28) < 1 && Math.abs(panel.left + panel.width / 2 - timeline.left - timeline.width / 2) < 1);
    }
    window.__setChatWidth('full');
    check('Full removes the column limit', Math.abs(column() - messages.clientWidth + 28) < 1 && column() > 72 * rem);
    check('Full input panel fills the pane', Math.abs(footer.getBoundingClientRect().width - messages.clientWidth) < 1);
    document.body.style.width = '400px';
    window.__setChatWidth('narrow');
    check('narrow panes retain the 14px edge', getComputedStyle(messages).paddingLeft === '14px' && getComputedStyle(messages).paddingRight === '14px');
    document.body.style.width = originalWidth;
    window.__setChatWidth('medium');
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

    // TRU-256: exercise the real find controls and native-key IPC seam.
    reset();
    const findBar = document.getElementById('chat-find');
    const query = document.getElementById('find-query');
    const count = document.getElementById('find-count');
    const tools = document.getElementById('find-tools');
    const findPosts = [];
    window.ipc = { postMessage: (message) => {
      const value = JSON.parse(message);
      findPosts.push(value);
      if (value.type === 'hostkey' && value.key === 'f') window.__openFind();
    } };
    const key = (target, value, options = {}) => {
      const event = new KeyboardEvent('keydown', { key: value, bubbles: true, cancelable: true, ...options });
      target.dispatchEvent(event);
      return event;
    };
    const search = (text) => { query.value = text; query.dispatchEvent(new Event('input', { bubbles: true })); };
    const highlights = () => [...(CSS.highlights.get('chat-find') || [])];
    const current = () => [...(CSS.highlights.get('chat-find-current') || [])][0];
    send({ type: 'user_prompt', text: 'Needle first' },
      H('text_delta', 'An earlier paragraph.\n\n'.repeat(70) + 'A **nee**dle and `NEEDLE`.\n\n```js\nconst needle = 1;\n```'),
      start('find-tool', 'Bash', { command: 'echo tool-only' }),
      H('item_completed', { id: 'find-tool', output: 'Needle tool-only', is_error: false }),
      H('thinking_delta', 'Needle thought'), H('text_delta', 'Needle final'), end);
    await settle();
    const originalText = messages.textContent;
    const originalCode = messages.querySelector('pre code').textContent;
    const originalFolds = new Map([...messages.querySelectorAll('details')].map((node) => [node, node.open]));
    const cmdF = key(document.getElementById('prompt'), 'f', { metaKey: true });
    check('Cmd+F forwards through hostkey and opens a labelled focused input', cmdF.defaultPrevented && findPosts.at(-1)?.key === 'f' && !findBar.hidden && document.activeElement === query && !!query.getAttribute('aria-label') && count.getAttribute('aria-live') === 'polite');
    search('needle');
    check('find highlights prose, thinking and code across inline markup with a live count', highlights().length === 6 && highlights().every((r) => r.toString().toLowerCase() === 'needle') && count.textContent === '1 of 6');
    const firstFind = current().startContainer;
    const foldedFind = document.querySelector('.turn-fold');
    check('first match leaves later folded content closed', !foldedFind.open);
    key(query, 'Enter');
    const secondFind = current().startContainer;
    const hitRect = current().getBoundingClientRect(), messageRect = messages.getBoundingClientRect();
    check('Enter advances in conversation order', count.textContent === '2 of 6' && secondFind !== firstFind);
    check('navigation scrolls the matching line into view', messages.scrollTop > 0 && hitRect.top >= messageRect.top && hitRect.bottom <= messageRect.bottom);
    key(query, 'Enter', { shiftKey: true });
    check('Shift+Enter returns to previous match', count.textContent === '1 of 6' && current().startContainer === firstFind);
    key(query, 'Enter', { shiftKey: true });
    check('previous wraps to the last match', count.textContent === '6 of 6' && current().startContainer.parentElement.closest('.assistant')?.textContent.trim() === 'Needle final');
    key(query, 'Enter');
    check('next wraps to the first match', count.textContent === '1 of 6');
    search('Needle thought');
    check('a match inside a folded turn and thinking card opens both', count.textContent === '1 of 1' && foldedFind.open && current().startContainer.parentElement.closest('details.thinking').open);
    search('tool-only');
    check('tool input and result excluded by default', count.textContent === '0 of 0' && !highlights().length && document.getElementById('find-next').disabled);
    tools.checked = true; tools.dispatchEvent(new Event('change'));
    check('toggle includes tool input and result and opens nested cards', count.textContent === '1 of 2' && highlights().length === 2 && card('find-tool').open && card('find-tool').closest('.tool-run').open);
    for (const size of [11, 22]) {
      window.__setChatFontSize(size);
      check('find controls scale with chat text at ' + size, [query, count, findBar.querySelector('label'), document.getElementById('find-next')].every((n) => Math.abs(parseFloat(getComputedStyle(n).fontSize) - size * 12 / 14) < 0.01));
    }
    window.__setChatFontSize(15);
    search('needle');
    check('range highlights leave code copy text unchanged', messages.querySelector('pre code').textContent === originalCode);
    const beforeEscape = findPosts.length;
    key(query, 'Escape');
    check('Esc restores exactly the folds find opened, preserving user-open folds', [...originalFolds].every(([node, open]) => node.open === open) && [...originalFolds.values()].some(Boolean));
    check('Esc clears every highlight, restores exact transcript text and focuses composer', findBar.hidden && !CSS.highlights.has('chat-find') && !CSS.highlights.has('chat-find-current') && messages.textContent === originalText && document.activeElement.id === 'prompt' && findPosts.length === beforeEscape);

    reset();
    const historyForFind = [];
    for (let i = 0; i < 1050; i++) historyForFind.push({ type: 'user_prompt', text: i === 0 ? 'Earlier [a.*] needle' : 'Historical message ' + i + (i === 300 || i === 1000 ? ' needle' : '') });
    window.__replay(historyForFind);
    check('history find fixture starts with unloaded earlier messages', !!document.querySelector('.earlier') && !messages.textContent.includes('Earlier [a.*] needle'));
    const historyNodes = [...messages.children];
    const earlierControl = messages.querySelector('.earlier');
    const earlierText = earlierControl.textContent;
    window.__openFind(); search('no such history text');
    check('zero-match search leaves all detached history out of layout', messages.childElementCount === historyNodes.length && earlierControl.textContent === earlierText);
    search('Historical message');
    check('all matches in the selected history chunk retain their ranges after attachment', highlights().length === 199 && highlights().every((range) => range.toString() === 'Historical message'));
    search('[a.*]');
    check('find attaches only the current history chunk and treats query literally', messages.childElementCount === historyNodes.length + 50 && count.textContent === '1 of 1' && current().toString() === '[a.*]' && earlierControl.textContent === earlierText);
    search('needle');
    key(query, 'Enter');
    check('navigating distant history replaces rather than accumulates chunks', count.textContent === '2 of 3' && messages.childElementCount === historyNodes.length + 50 && !messages.textContent.includes('Earlier [a.*] needle'));
    key(query, 'Enter');
    check('navigating back to live history detaches the temporary chunk', count.textContent === '3 of 3' && messages.childElementCount === historyNodes.length);
    key(query, 'Enter');
    key(query, 'Escape');
    check('closing restores exact history window and earlier control', messages.childElementCount === historyNodes.length && [...messages.children].every((node, i) => node === historyNodes[i]) && earlierControl.textContent === earlierText && !earlierControl.disabled);
    earlierControl.click();
    check('normal history paging works after find closes', messages.childElementCount === historyNodes.length + 50 && messages.textContent.includes('Historical message 899') && !messages.textContent.includes('Earlier [a.*] needle'));
    // Trimming resumes after find, including when the user has paged back.
    jump.click();
    check('following after find trims to the original live window', messages.childElementCount === historyNodes.length);

    // A roster jump pages across the chunk temporarily borrowed by find.
    window.__resetConversation();
    const rosterHistory = [];
    for (let i = 0; i < 1050; i++) {
      rosterHistory.push({ type: 'user_prompt', text: 'Roster history message ' + i });
      if (i === 250) rosterHistory.push(H('text_delta', '**rosterdeferredneedle**'), end);
      if (i === 10) rosterHistory.push({ kind: 'delegation', delegation: {
        delegation_id: 'history-worker', kind: 'implement', brief: 'Check history',
        child: { model: 'test-model' }, status: { state: 'completed' },
        created_at: '2026-11-01T10:00:00Z', updated_at: '2026-11-01T10:01:00Z',
        result: { handoff: { summary: 'Historical report', decisions: [], next_steps: [], blockers: [] } },
      } });
    }
    window.__replay(rosterHistory);
    const rosterWindow = [...messages.children];
    const rosterEarlierText = messages.querySelector('.earlier').textContent;
    window.__openFind(); search('Roster history message 300');
    document.querySelector('#roster [data-key="dlg:history-worker"]').click();
    check('roster jump during find reveals an older worker across the borrowed chunk', !!messages.querySelector('[data-delegation-id="history-worker"]'));
    key(query, 'Escape');
    check('Esc after roster paging restores the exact history window', [...messages.children].length === rosterWindow.length && [...messages.children].every((node, i) => node === rosterWindow[i]) && messages.querySelector('.earlier')?.textContent === rosterEarlierText);
    window.__openFind(); search('Roster history message 300');
    // Exercise the shared paging handler even though the visible button is
    // disabled during find, then borrow another chunk before the roster jump.
    messages.querySelector('.earlier').dispatchEvent(new Event('click'));
    search('Roster history message 200');
    document.querySelector('#roster [data-key="dlg:history-worker"]').click();
    jump.click();
    key(query, 'Escape');
    check('multiple paging paths and trimming during find restore the same window', [...messages.children].length === rosterWindow.length && [...messages.children].every((node, i) => node === rosterWindow[i]) && messages.querySelector('.earlier')?.textContent === rosterEarlierText);
    window.__openFind(); search('rosterdeferredneedle');
    check('deferred markdown remains searchable after roster paging restoration', count.textContent === '1 of 1' && current().startContainer.parentElement.tagName === 'STRONG');
    key(query, 'Escape');
    window.__openFind(); search('Roster history message ');
    check('search after roster paging still finds every historical message', count.textContent === '1 of 1050');
    key(query, 'Escape');
    while (messages.querySelector('.earlier')) messages.querySelector('.earlier').click();
    const pagedHistory = [...messages.querySelectorAll('.user-msg')].map((node) => node.textContent);
    check('paging after roster find reaches every message in transcript order', pagedHistory.length === 1050 && pagedHistory.every((text, i) => text === 'Roster history message ' + i));

    reset();
    const replayForFind = [];
    for (let i = 0; i < 220; i++) replayForFind.push({ type: 'user_prompt', text: 'Question ' + i }, H('text_delta', i === 0 ? '**deferredneedle**' : 'Answer ' + i), end);
    window.__resetConversation(); window.__replay(replayForFind);
    window.__openFind(); search('deferredneedle');
    const deferredRoot = current().startContainer.parentElement.closest('.assistant');
    check('find renders only the selected deferred block in its chunk', count.textContent === '1 of 1' && current().startContainer.parentElement.tagName === 'STRONG' && [...messages.querySelectorAll('.assistant')].filter((node) => node !== deferredRoot && !node.textContent).length > 0);
    key(query, 'Escape');
    check('closing restores deferred markdown and detached block state', !deferredRoot.isConnected && !deferredRoot.textContent);
    window.__openFind(); search('deferredneedle');
    check('restored deferred source is searchable on a later find', count.textContent === '1 of 1' && current().toString() === 'deferredneedle');
    key(query, 'Escape');

    reset();
    const longOutput = 'x'.repeat(21000) + ' beyondneedle';
    send(start('long-find'), H('item_completed', { id: 'long-find', output: longOutput, is_error: false }), end);
    const truncatedText = messages.textContent;
    window.__openFind(); search('beyondneedle');
    check('long tool result stays excluded by default', count.textContent === '0 of 0');
    // Scope is retained across opening/closing find, so set explicitly here.
    tools.checked = true; tools.dispatchEvent(new Event('change'));
    check('find includes full tool output beyond display truncation', count.textContent === '1 of 1' && current().toString() === 'beyondneedle' && card('long-find').open);
    key(query, 'Escape');
    check('closing restores truncated tool result text exactly', messages.textContent === truncatedText);

    reset();
    send({ kind: 'delegation', delegation: {
      delegation_id: 'find-report', kind: 'implement', brief: 'Check the project',
      child: { model: 'test-model' }, status: { state: 'completed' },
      created_at: '2026-11-01T10:00:00Z', updated_at: '2026-11-01T10:00:00Z',
      result: { handoff: { summary: 'Cardneedle summary', decisions: ['Cardneedle decision'], next_steps: ['Cardneedle next step'], blockers: [] } },
    } });
    const report = messages.querySelector('.dlg'); report.open = false;
    window.__openFind(); search('cardneedle');
    check('default scope searches agent reports inside closed delegation cards', count.textContent === '1 of 3' && report.open);
    key(query, 'Escape');
    send(start('find-subagent', 'Agent', { description: 'Check the project' }), H('subagent_event', { parent_tool_use_id: 'find-subagent', event: { type: 'text_delta', data: 'Subagentneedle response' } }));
    card('find-subagent').open = false;
    card('find-subagent').querySelector('.subagent').open = false;
    window.__openFind(); search('subagentneedle');
    check('default scope searches subagent writing inside closed tool cards', count.textContent === '1 of 1' && card('find-subagent').open && card('find-subagent').querySelector('.subagent').open);
    key(query, 'Escape');

    reset();
    const detachedWorker = {
      delegation_id: 'detached-find-report', kind: 'implement', brief: 'Check history',
      child: { model: 'test-model' }, status: { state: 'running' },
      created_at: '2026-11-01T10:00:00Z', updated_at: '2026-11-01T10:00:00Z',
    };
    window.__replay([
      { kind: 'delegation', delegation: detachedWorker },
      ...Array.from({ length: 250 }, (_, i) => ({ type: 'user_prompt', text: 'Later prompt ' + i })),
    ]);
    check('late worker fixture starts in detached history', !!messages.querySelector('.earlier') && !messages.querySelector('[data-delegation-id="detached-find-report"]'));
    window.__openFind(); search('lateworkerneedle');
    check('unfinished detached report has no matches', count.textContent === '0 of 0');
    await settle(); await settle();
    send({ kind: 'delegation', delegation: {
      ...detachedWorker, status: { state: 'completed' },
      result: { handoff: { summary: 'lateworkerneedle report', decisions: [], next_steps: [], blockers: [] } },
    } });
    await settle(); await settle();
    check('completion in detached history refreshes find without retyping', count.textContent === '1 of 1' && current()?.toString() === 'lateworkerneedle');
    key(query, 'Escape');

    reset();
    send({ kind: 'delegation', delegation: {
      delegation_id: 'find-review', kind: 'review', brief: 'Check the project',
      child: { model: 'test-model' }, status: { state: 'completed' },
      created_at: '2026-11-01T10:00:00Z', updated_at: '2026-11-01T10:00:00Z',
      result: { findings: { verdict: 'needs_changes', summary: 'Review complete', structured: true,
        findings: [{ id: 'F1', severity: 'P2', title: 'Reviewtitleneedle', body: 'Details' }] } },
    } });
    window.__openFind(); search('reviewtitleneedle');
    check('review finding title counts once despite nested search roots', count.textContent === '1 of 1' && highlights().length === 1);
    key(query, 'Escape');

    reset();
    send({ type: 'turn_end', message: { content: [
      { type: 'thinking', thinking: 'Pi thinkingneedle' },
      { type: 'toolCall', name: 'Bash', arguments: { command: 'Pi toolneedle' } },
    ] }, toolResults: ['Pi toolneedle result'] });
    window.__openFind(); search('thinkingneedle');
    check('Pi thinking is included in default search scope', count.textContent === '1 of 1');
    search('toolneedle');
    check('Pi tool calls and results are excluded by default', count.textContent === '0 of 0');
    tools.checked = true; tools.dispatchEvent(new Event('change'));
    check('Pi tool calls and results are included with the toggle', count.textContent === '1 of 2');
    key(query, 'Escape');

    reset();
    send(H('text_delta', 'needle [link](https://needle.test) needle chosen needle last'));
    await settle();
    window.__openFind(); search('needle'); key(query, 'Enter'); key(query, 'Enter');
    check('streaming markdown fixture selects the occurrence before chosen', count.textContent === '3 of 4' && current().startContainer.textContent.slice(current().startOffset).startsWith('needle chosen'));
    send(H('text_delta', '\n'));
    await settle(); await settle();
    check('markdown conversion preserves the selected occurrence when URL offsets disappear', count.textContent === '2 of 3' && current().startContainer.textContent.slice(current().startOffset).startsWith('needle chosen'));
    key(query, 'Escape');

    reset();
    send(H('text_delta', 'needle [link](https://needle.test) **needle** chosen needle last'));
    await settle();
    window.__openFind(); search('needle'); key(query, 'Enter'); key(query, 'Enter');
    check('inline markdown fixture selects the bold source occurrence', count.textContent === '3 of 4' && current().startContainer.textContent.slice(current().startOffset).startsWith('needle** chosen'));
    send(H('text_delta', '\n'));
    await settle(); await settle();
    check('inline markdown conversion preserves the selected bold occurrence', count.textContent === '2 of 3' && current().startContainer.parentElement.tagName === 'STRONG' && current().toString() === 'needle');
    key(query, 'Escape');

    // Run only the streaming paint, then navigate before the find frame.
    for (const backwards of [false, true]) {
      reset();
      send(H('text_delta', 'needle [link](https://needle.test) needle chosen needle last'));
      await settle();
      window.__openFind(); search('needle'); key(query, 'Enter'); key(query, 'Enter');
      await settle(); await settle();
      const quietNode = messages.querySelector('.user-msg').firstChild;
      const dataProperty = Object.getOwnPropertyDescriptor(CharacterData.prototype, 'data');
      let quietReads = 0;
      Object.defineProperty(quietNode, 'data', { configurable: true, get() { quietReads++; return dataProperty.get.call(this); } });
      send(H('text_delta', '\n'));
      frames.shift()(performance.now());
      // Cover both undelivered records and a delivered observer callback whose
      // refresh frame has not run yet.
      if (backwards) await Promise.resolve();
      key(query, 'Enter', { shiftKey: backwards });
      check('navigation leaves unchanged blocks unscanned (' + (backwards ? 'previous' : 'next') + ')', quietReads === 0);
      delete quietNode.data;
      check('navigation refreshes streamed matches before the observer frame (' + (backwards ? 'previous' : 'next') + ')', count.textContent === (backwards ? '1 of 3' : '3 of 3') && current().startContainer.textContent.slice(current().startOffset).startsWith(backwards ? 'needle ' : 'needle last'));
      key(query, 'Escape');
      await settle();
    }

    reset();
    send(H('text_delta', '**prefix** ' + 'needle repeated '.repeat(12)));
    await settle();
    window.__openFind(); search('needle');
    for (let i = 0; i < 5; i++) key(query, 'Enter');
    send(H('text_delta', '\n'));
    await settle(); await settle();
    check('identical context after a markdown prefix preserves occurrence six', count.textContent === '6 of 12');
    key(query, 'Escape');

    reset();
    send(H('text_delta', 'needle repeated '.repeat(12)));
    await settle();
    window.__openFind(); search('needle');
    for (let i = 0; i < 5; i++) key(query, 'Enter');
    send(H('text_delta', '\n'));
    await settle(); await settle();
    check('identical streaming context keeps the nearest occurrence in its block', count.textContent === '6 of 12');
    key(query, 'Escape');

    reset(); tools.checked = false;
    send(H('text_delta', 'Needle first\nNeedle second'));
    await settle();
    window.__openFind(); search('needle'); key(query, 'Enter');
    const streamRoot = current().startContainer.parentElement.closest('.assistant');
    const streamTop = messages.scrollTop;
    send(H('text_delta', '\nNeedle third'));
    await settle(); await settle();
    check('streaming updates count without losing selected match or scroll', count.textContent === '2 of 3' && current().startContainer.parentElement.closest('.assistant') === streamRoot && messages.scrollTop === streamTop && current().toString() === 'Needle');
    send(start('stream-find'), done('stream-find'), H('text_delta', 'All finished.'), end);
    await settle();
    check('turn folding keeps the current streaming match expanded', count.textContent === '2 of 3' && streamRoot.closest('.turn-fold')?.open);
    key(query, 'Escape');

    reset();
    send(H('text_delta', 'Earlier line.\n'.repeat(100) + 'needle at the end'));
    await settle();
    window.__openFind(); search('needle');
    const beforeGrowth = messages.scrollTop;
    messages.dispatchEvent(new Event('scroll')); // Reaching bottom must not restart live follow.
    send(H('text_delta', '\nNew streaming line.'.repeat(100)));
    await settle(); await settle();
    check('stream growth at the bottom cannot scroll the selected match away', beforeGrowth > 0 && messages.scrollTop === beforeGrowth && count.textContent === '1 of 1' && current().getBoundingClientRect().bottom <= messages.getBoundingClientRect().bottom);
    key(query, 'Escape');

    reset();
    send(H('text_delta', '```js\nconst ' + 'x'.repeat(300) + ' = "horizontalneedle";\n```'), end);
    window.__openFind(); search('horizontalneedle');
    const codeScroller = current().startContainer.parentElement.closest('pre');
    const codeHit = current().getBoundingClientRect(), codeBox = codeScroller.getBoundingClientRect();
    check('find scrolls long code lines horizontally into view', codeScroller.scrollLeft > 0 && codeHit.left >= codeBox.left && codeHit.right <= codeBox.right);
    key(query, 'Escape');

    check('closing restores horizontal code scroll', codeScroller.scrollLeft === 0);

    reset();
    send(H('text_delta', 'Follow test line.\n'.repeat(100)));
    await settle();
    jump.click();
    window.__openFind(); search('Follow test'); key(query, 'Escape');
    await settle();
    send(H('text_delta', 'New followed line.\n'.repeat(20)));
    await settle();
    check('following before find resumes following after Esc and stream growth', messages.scrollHeight - messages.clientHeight - messages.scrollTop < 2 && !jump.classList.contains('visible'));
    messages.dispatchEvent(new Event('wheel'));
    messages.scrollTop = 100;
    messages.dispatchEvent(new Event('scroll'));
    const readerTop = messages.scrollTop;
    window.__openFind(); search('New followed'); key(query, 'Escape');
    await settle();
    send(H('text_delta', 'Additional line.\n'.repeat(20)));
    await settle();
    check('a reader not following before find keeps their original position after Esc', readerTop === 100 && messages.scrollTop === readerTop && jump.classList.contains('visible'));

    // Older supported WebKit must search without the CSS Highlight API.
    reset();
    send(H('text_delta', 'Legacyneedle first and legacyneedle second.'), end);
    const oldWebkitText = messages.textContent;
    const highlightApi = window.Highlight;
    const highlightRegistry = Object.getOwnPropertyDescriptor(CSS, 'highlights');
    window.Highlight = undefined;
    Object.defineProperty(CSS, 'highlights', { value: undefined, configurable: true });
    window.__openFind(); search('legacyneedle');
    await settle();
    check('older WebKit paints range overlays without altering transcript text', count.textContent === '1 of 2' && document.querySelectorAll('.find-hit').length >= 2 && !!document.querySelector('.find-hit.current') && messages.textContent === oldWebkitText);
    key(query, 'Enter');
    check('older WebKit retains keyboard navigation', count.textContent === '2 of 2');
    key(query, 'Escape');
    check('older WebKit clears overlays and preserves exact transcript text', !document.querySelector('.find-hit') && findBar.hidden && messages.textContent === oldWebkitText);
    window.Highlight = highlightApi;
    Object.defineProperty(CSS, 'highlights', highlightRegistry);

    // Leave a compact synthetic example for --screenshot.
    reset();
    send({ type: 'user_prompt', text: 'Where is the chat search implemented?' },
      H('text_delta', 'The **chat search** lives in the current conversation.\n\nIt finds text in earlier messages, folded turns, and code:\n\n```js\nconst search = "plain text";\n```'),
      start('screenshot-search'), done('screenshot-search'), H('text_delta', 'Use Enter to move to the next search match, or Shift+Enter to go back.'), end);
    await settle();
    window.__openFind(); search('search'); key(query, 'Enter');

    check('no event-render errors', ![...document.querySelectorAll('.error-block')].some((n) => n.textContent.startsWith('Chat view could not render')));
  } catch (error) {
    results.push('FAIL ' + error.stack);
  }
  window.requestAnimationFrame = requestFrame;
  const result = document.createElement('pre');
  result.id = 'live-turn-test-result';
  result.hidden = true;
  result.textContent = results.join('\n');
  document.body.appendChild(result);
})();
