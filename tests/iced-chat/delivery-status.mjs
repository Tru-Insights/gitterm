// Exercise the page's queue presentation functions without a browser or startup.
import assert from 'node:assert/strict';
import fs from 'node:fs';
import vm from 'node:vm';
const source = fs.readFileSync(new URL('../../assets/agent_chat.html', import.meta.url), 'utf8');
function fn(name, next) {
  const start = source.indexOf(`  function ${name}(`);
  assert(start >= 0);
  return source.slice(start, source.indexOf(next, start));
}
function bubble() {
  const classes = new Set(['queued']);
  const children = [];
  return { classes, children,
    classList: { add: x => classes.add(x), remove: x => classes.delete(x) },
    append: x => children.push(x),
    remove() { this.removed = true; },
    querySelector: () => children.find(x => !x.removed && x.cls.includes('delivery-uncertain')),
  };
}
const unknown = bubble(), known = bubble();
const queued = new Map([['unknown', {msg: unknown, tag: {remove() {}}, label: {}, x: {remove() {}}}], ['known', {msg: known, label: {}, x: {remove() {}}}]]);
const notices = [];
const context = vm.createContext({
  mode: { pending: null }, chips: { model: { pending: null }, effort: { pending: null } },
  renderMode() {}, renderModel() {}, renderEffort() {},
  prompts: new Map([['unknown', unknown], ['known', known]]), queued,
  el: (_tag, cls, text) => ({ cls, text, remove() { this.removed = true; } }),
  harnessName: () => 'Agent', note: x => notices.push(x),
  queuedMarkerAction: state => state === 'started' ? 'delivered' : '',
  markQueued: (msg, id) => { msg.classList.add('queued'); queued.set(id, { msg, tag: {remove() {}} }); },
});
vm.runInContext(fn('promptUncertain', '  // ---- entry points'), context);
vm.runInContext(fn('dropQueued', '  // ---- harness events'), context);
vm.runInContext(fn('queuedLifecycle', '  function queuedWithdrawRefused'), context);
vm.runInContext(fn('queuedWithdrawRefused', '  // Claude exited:'), context);
vm.runInContext("promptUncertain('unknown'); promptUncertain('unknown'); dropQueued();", context);
assert.equal(unknown.children.length, 1);
assert.match(unknown.children[0].text, /^Delivery uncertain:/);
assert.equal(unknown.classes.has('queued'), false);
assert.equal(known.classes.has('unsent'), true);
assert.equal(notices.length, 1);
assert.match(notices[0], /A queued message/);
vm.runInContext("queuedLifecycle('unknown', 'started')", context);
assert.equal(unknown.children[0].removed, true);
assert.equal(unknown.classes.has('unsent'), false);
vm.runInContext("queuedLifecycle('known', 'completed')", context);
assert.equal(known.classes.has('unsent'), true);
vm.runInContext("promptUncertain('unknown'); queuedLifecycle('unknown', 'queued')", context);
assert.equal(unknown.classes.has('queued'), true);
assert.equal(unknown.classes.has('unsent'), false);
vm.runInContext("promptUncertain('unknown'); queuedLifecycle('unknown', 'cancelled')", context);
assert.equal(unknown.removed, true);
vm.runInContext("queuedWithdrawRefused('already-started', 'Not withdrawn: already started')", context);
assert.equal(notices.at(-1), 'Not withdrawn: already started');
vm.runInContext("pickerPending({setting:'mode',value:'plan'})", context);
assert.equal(context.mode.pending, 'plan');
vm.runInContext("pickerPending({setting:'effort',value:null})", context);
assert.equal(context.chips.effort.pending.value, null);
assert.match(source, /event.kind === 'prompt_unsent'.*__markPromptUnsent/);
console.log('Iced queued/uncertain delivery presentation passed');

// A typed rejection is silent; the existing Error renders exactly its data.
const errors = [];
context.append = block => errors.push(block);
context.endSegment = () => {};
vm.runInContext(fn('clearPendingMode', '  function requestMode'), context);
vm.runInContext(fn('clearPendingChips', '  function requestModel'), context);
vm.runInContext(fn('errorBlock', '  function harnessEvent'), context);
vm.runInContext(fn('harnessEvent', '  // ---- legacy'), context);
for (const [requested, message] of [
  [{kind:'model',value:'sonnet'}, 'Claude rejected model "sonnet": denied'],
  [{kind:'mode',value:'plan'}, 'Claude rejected permission mode "plan": denied'],
  [{kind:'effort',value:'high'}, 'Claude did not change the effort to high: denied'],
  [{kind:'effort',value:null}, 'Claude did not change the effort to auto: denied'],
]) {
  context.mode.current = 'default';
  context.chips.model.current = 'default';
  context.chips.effort.current = 'medium';
  context.mode.pending = 'plan';
  context.chips.model.pending = 'sonnet';
  context.chips.effort.pending = {value: 'high'};
  context.rejection = {type:'setting_rejected',data:{requested}};
  context.error = {type:'error',data:message};
  const count = errors.length;
  vm.runInContext('harnessEvent(rejection)', context);
  assert.equal(errors.length, count);
  vm.runInContext('harnessEvent(error)', context);
  assert.equal(errors.length, count + 1);
  assert.equal(errors.at(-1).cls, 'error-block');
  assert.equal(errors.at(-1).text, message);
  assert.equal(context.mode.pending, null);
  assert.equal(context.chips.model.pending, null);
  assert.equal(context.chips.effort.pending, null);
  assert.equal(context.mode.current, 'default');
  assert.equal(context.chips.model.current, 'default');
  assert.equal(context.chips.effort.current, 'medium');
}
console.log('Iced rejection error text and picker presentation passed');
