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
console.log('Iced queued/uncertain delivery presentation passed');
