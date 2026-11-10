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
      // Both reflow controls retain live-follow at the bottom.
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
      window.__setChatWidth('narrow');
      await settle();
      check('following view stays at the bottom after a width change', messages.scrollHeight - messages.scrollTop - messages.clientHeight < 2);
      for (const div of fillers) div.remove();
    }
    {
      // One reply can span many screens, including within a single paragraph.
      // Its container's pixel offset cannot identify the line being read.
      reset();
      const paragraph = 'The reading position should survive text reflow through the same reply. '.repeat(30);
      send(H('text_delta', Array.from({ length: 60 }, (_, i) => 'Paragraph ' + i + ': ' + paragraph).join('\n\n')), end);
      const reply = messages.querySelector('.assistant');
      const text = reply.querySelectorAll('p')[30].firstChild;
      check('reflow fixture is one long reply', messages.querySelectorAll('.assistant').length === 1 && reply.querySelectorAll('p').length === 60);
      for (const withinParagraph of [false, true]) {
        for (const [name, width, size, change] of [
          ['15px to 22px', 'wide', 15, () => window.__setChatFontSize(22)],
          ['22px to 11px', 'wide', 22, () => window.__setChatFontSize(11)],
          ['Wide to Narrow', 'wide', 15, () => window.__setChatWidth('narrow')],
          ['Narrow to Wide', 'narrow', 15, () => window.__setChatWidth('wide')],
        ]) {
          window.__setChatWidth(width);
          window.__setChatFontSize(size);
          await settle();
          const range = document.createRange();
          let offset = withinParagraph ? Math.floor(text.length / 2) : 0;
          const rect = () => {
            range.setStart(text, offset);
            range.setEnd(text, offset + 1);
            return range.getBoundingClientRect();
          };
          // Identify the first character of this rendered line independently
          // of keepView, then leave that line partially visible at the top.
          const lineTop = rect().top;
          while (offset > 0) {
            offset--;
            if (rect().top !== lineTop) { offset++; break; }
          }
          const line = rect();
          messages.dispatchEvent(new WheelEvent('wheel', { deltaY: -400 }));
          messages.scrollTop += line.top - messages.getBoundingClientRect().top + line.height / 2;
          messages.dispatchEvent(new Event('scroll'));
          await settle();
          const before = rect().top;
          change();
          await settle();
          check('long reply retains ' + (withinParagraph ? 'mid-paragraph line' : 'paragraph') + ' after ' + name,
            Math.abs(rect().top - before) < 2 && jump.classList.contains('visible'));
        }
      }
      reset();
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
