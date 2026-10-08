// Optional Node.js development test; no packages or frontend runtime required.
// Run: node scripts/tests/clipboard.cjs
const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const vm = require('node:vm');
const source = fs.readFileSync(path.join(__dirname, '../../static/app.js'), 'utf8');

async function check({ modern, legacy, succeeds = true }) {
  let copied = null;
  let input = null;
  let removed = false;
  let restored = false;
  const status = { textContent: '' };
  const label = { textContent: 'Copy' };
  const button = {
    hidden: true, disabled: false,
    dataset: { copyModel: 'openai/vendor/model' },
    querySelector: () => label,
    classList: { add() {}, remove() {} },
  };
  const document = {
    activeElement: { focus() { restored = true; } },
    body: { addEventListener() {}, appendChild(element) { input = element; } },
    addEventListener() {},
    querySelectorAll: (selector) => selector === '[data-copy-model]' ? [button] : [],
    getElementById: (id) => id === 'copy-status' ? status : null,
    createElement: () => ({
      focus() {}, select() {}, remove() { removed = true; },
    }),
  };
  if (legacy) document.execCommand = (command) => {
    assert.equal(command, 'copy');
    copied = input.value;
    return succeeds;
  };
  const navigator = {};
  if (modern) navigator.clipboard = { async writeText(text) {
    if (modern === 'reject') throw new Error('Permission denied');
    copied = text;
  } };
  const timers = new Map();
  let nextTimer = 0;
  const window = {
    setTimeout(callback, delay) { timers.set(++nextTimer, { callback, delay }); return nextTimer; },
    clearTimeout(id) { timers.delete(id); },
  };
  const context = vm.createContext({ document, navigator, window });
  vm.runInContext(source, context);
  assert.equal(button.hidden, !(modern || legacy));
  if (!modern && !legacy) return;
  await context.copyModelId(button);
  assert.equal(copied, 'openai/vendor/model');
  assert.equal(button.disabled, false);
  assert.equal(label.textContent, succeeds ? 'Copied' : 'Copy failed');
  assert.equal(status.textContent, succeeds ? 'Text copied' : 'Could not copy. Copy the model ID manually.');
  await context.copyModelId(button);
  const toastTimers = [...timers.values()].filter((timer) => timer.delay === 2000);
  assert.equal(toastTimers.length, 1, 'repeated copies must reset the toast timer');
  toastTimers[0].callback();
  assert.equal(status.textContent, '', 'toast must disappear after its timeout');
  if (legacy && modern !== 'success') {
    assert.equal(removed, true, 'temporary textarea must be removed');
    assert.equal(restored, true, 'original focus must be restored');
  }
}

(async () => {
  await check({ modern: 'success' });
  await check({ legacy: true });
  await check({ modern: 'reject', legacy: true });
  await check({ legacy: true, succeeds: false });
  await check({});
  console.log('Clipboard tests passed: modern, HTTP fallback, rejection, cleanup, visibility, and toast lifecycle.');
})().catch((error) => { console.error(error); process.exitCode = 1; });
