const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const test = require('node:test');
const vm = require('node:vm');

const script = fs.readFileSync(path.join(__dirname, '../assets/review_pane.js'), 'utf8');

// A fresh page after every click, as Rust replaces the review document.
function reload(selection) {
  const messages = [];
  const buttons = ['A', 'B', 'C', 'D'].map((sha) => ({
    dataset: { commit: sha },
    addEventListener(type, handler) { this.click = handler; },
  }));
  const document = {
    body: {
      dataset: { token: 'page', firstCommit: selection?.pending ? selection.first : '' },
      classList: { toggle() {} },
    },
    getElementById() { return null; },
    querySelectorAll(selector) { return selector === 'button[data-commit]' ? buttons : []; },
    addEventListener() {},
  };
  vm.runInNewContext(script, {
    document, window: { ipc: { postMessage(message) { messages.push(JSON.parse(message)); } } },
  });
  return (sha) => {
    buttons.find((button) => button.dataset.commit === sha).click({});
    return messages.at(-1);
  };
}

test('completed range resets the next click to a single commit across reloads', () => {
  let selected = reload(null)('A');
  assert.deepEqual([selected.first, selected.last, selected.pending], ['A', 'A', true]);
  selected = reload(selected)('B');
  assert.deepEqual([selected.first, selected.last, selected.pending], ['A', 'B', false]);
  selected = reload(selected)('C');
  assert.deepEqual([selected.first, selected.last, selected.pending], ['C', 'C', true]);
  selected = reload(selected)('D');
  assert.deepEqual([selected.first, selected.last, selected.pending], ['C', 'D', false]);
});

test('clicking the same endpoint twice completes the selection phase', () => {
  let selected = reload(null)('A');
  selected = reload(selected)('A');
  assert.equal(selected.pending, false);
  selected = reload(selected)('C');
  assert.deepEqual([selected.first, selected.last, selected.pending], ['C', 'C', true]);
});
