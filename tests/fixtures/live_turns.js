// Real streaming events against the chat page, without an agent process.
(() => {
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
    check('no event-render errors', ![...document.querySelectorAll('.error-block')].some((n) => n.textContent.startsWith('Chat view could not render')));
  } catch (error) {
    results.push('FAIL ' + error.stack);
  }
  const result = document.createElement('pre');
  result.id = 'live-turn-test-result';
  result.textContent = results.join('\n');
  document.body.appendChild(result);
})();
