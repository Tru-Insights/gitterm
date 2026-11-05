// Real card rendering and IPC; no agent or running GitTerm is involved.
(() => {
  const results = [];
  const check = (name, ok) => results.push((ok ? 'PASS ' : 'FAIL ') + name);
  const calls = [];
  window.ipc = { postMessage: (message) => calls.push(JSON.parse(message)) };
  window.__setTabId(1, 'default', { model: 'default' }, false);
  const record = (id, kind, state, extra = {}) => ({
    delegation_id: id, kind, brief: 'Check result delivery',
    child: { runner: kind === 'implement' ? 'task_session' : 'codex_consult', model: 'test-model' },
    status: { state, ...(state === 'failed' ? { message: 'Stopped at the time limit' } : {}) },
    created_at: '2026-11-01T10:00:00Z', updated_at: '2026-11-01T10:01:00Z',
    result: state === 'failed' ? undefined : kind === 'review'
      ? { findings: { verdict: 'needs_changes', summary: 'Review is ready.', structured: true, findings: [{ id: 'F1', severity: 'P1', title: 'A finding', body: 'Details' }] } }
      : { handoff: { summary: 'Result is ready.', decisions: [], next_steps: [], blockers: [] } },
    ...extra,
  });
  const append = (d, held = false) => {
    window.__appendEvent({ kind: 'delegation', delegation: d, held });
    return [...document.querySelectorAll('.dlg')].find((node) => node.dataset.delegationId === d.delegation_id);
  };
  const send = (card) => [...card.querySelectorAll('button')].find((button) => button.textContent === 'Send to Claude');
  try {
    for (const kind of ['review', 'consult', 'implement']) {
      const d = record('delivered-' + kind, kind, 'completed', { delivered: true });
      const card = append(d);
      check(kind + ' delivered is muted text with no send button',
        [...card.querySelectorAll('.dlg-note')].some((note) => note.textContent === 'sent to chat ✓') && !send(card));
      if (kind === 'review') check('delivered review disables finding selection', card.querySelector('input[type=checkbox]')?.disabled);
    }
    const blocked = append(record('blocked-record', 'implement', 'blocked', { delivered: true }));
    check('blocked delivered worker retains record and sent text', blocked.textContent.includes('sent to chat ✓') && !send(blocked));
    const legacy = append(record('legacy-record', 'consult', 'completed', { delivered_at: '2026-11-01T10:01:00Z' }));
    check('legacy timestamp renders sent text', legacy.textContent.includes('sent to chat ✓') && !send(legacy));
    const failed = append(record('failed-record', 'consult', 'failed', { delivered: true }));
    check('failure delivery shows sent text and preserves error', failed.textContent.includes('sent to chat ✓') && failed.textContent.includes('Stopped at the time limit'));
    const manual = append(record('manual-record', 'consult', 'completed'));
    check('undelivered card retains enabled manual button (automatic off or parent gone)', send(manual) && !send(manual).disabled);
    send(manual).click();
    check('manual button sends IPC', calls.some((call) => call.type === 'delegation_send' && call.delegationId === 'manual-record'));
    const pending = record('pending-record', 'consult', 'completed');
    const held = append(pending, true);
    check('busy chat shows disabled queued button', [...held.querySelectorAll('button')].some((button) => button.disabled && button.textContent.includes('Queued')));
    append({ ...pending, delivered: true });
    check('turn-end update replaces queued button with sent text', held.textContent.includes('sent to chat ✓') && ![...held.querySelectorAll('button')].some((button) => button.textContent.includes('Queued')));
    const undeliveredFailure = append(record('unsent-failure', 'review', 'failed'));
    check('failed undelivered card offers manual send', !!send(undeliveredFailure));
    const progress = append(record('progress-record', 'consult', 'running'));
    check('progress has no send button or sent text', !send(progress) && !progress.textContent.includes('sent to chat ✓'));
    check('rendering cards never sends automatically from JavaScript', calls.filter((call) => call.type === 'delegation_send').length === 1);
  } catch (error) {
    results.push('FAIL ' + error.stack);
  }
  for (const card of document.querySelectorAll('.dlg')) card.open = true;
  const result = document.createElement('pre');
  result.id = 'card-test-result';
  result.textContent = results.join('\n');
  document.body.appendChild(result);
})();
