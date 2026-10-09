// Dependency-free interaction checks; this is a DOM mock, not a visual browser test.
const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const vm = require('node:vm');

class Element {
  constructor() {
    this.value = '';
    this.hidden = false;
    this.children = [];
    this.listeners = {};
    this.style = {};
    this.scrollHeight = 64;
    this.textContent = '';
  }
  addEventListener(name, fn) { this.listeners[name] = fn; }
  append(...nodes) { this.children.push(...nodes); }
  replaceChildren() { this.children = []; }
  setAttribute() {}
  removeAttribute() {}
  focus() { this.focused = true; }
  scrollIntoView() { this.scrolled = true; }
  get lastElementChild() { return this.children.at(-1); }
  querySelector() { return empty; }
  querySelectorAll() {
    return this.children.map(node => ({ remove: () => {
      this.children = this.children.filter(child => child !== node);
    } }));
  }
}
const ids = ['test-form', 'test-csrf', 'test-combo', 'test-key', 'test-draft',
  'test-send', 'test-reset', 'test-status', 'test-error', 'test-log', 'test-attempt-list',
  'test-mode', 'test-evaluation', 'test-state', 'test-questions'];
const nodes = Object.fromEntries(ids.map(id => [id, new Element()]));
const empty = new Element();
const attemptsEmpty = new Element();
const windowListeners = {};
const requests = [];
let fail = false;
let resolveFetch;
nodes['test-mode'].value = 'chat';
nodes['test-combo'].value = 'coding';
nodes['test-csrf'].value = 'csrf-fixture';
nodes['test-key'].value = 'gateway-fixture';
vm.runInNewContext(fs.readFileSync(path.join(__dirname, '../../static/test-chat.js'), 'utf8'), {
  document: {
    getElementById: id => nodes[id],
    querySelector: () => attemptsEmpty,
    createElement: () => new Element(),
  },
  window: { addEventListener: (name, fn) => { windowListeners[name] = fn; } },
  AbortController,
  crypto: require('node:crypto').webcrypto,
  fetch: (url, options) => {
    requests.push({ url, options, body: JSON.parse(options.body) });
    return new Promise(resolve => {
      resolveFetch = () => resolve({ ok: !fail, status: fail ? 502 : 200,
        json: async () => fail
          ? { error: { message: 'Provider unavailable' }, attempts: [] }
          : { completion: url === '/test/systemone'
              ? { model: 'jev-1.13.0', answers: { urgent: { type: 'noul', noul: 0.95 } }, extension: '<script>safe</script>' }
              : { choices: [{ message: { content: '<script>plain text</script>' } }] }, attempts: [] },
      });
    });
  },
});
const draft = nodes['test-draft'];
function key(overrides = {}) {
  let prevented = false;
  draft.listeners.keydown({ key: 'Enter', preventDefault: () => { prevented = true; }, ...overrides });
  return prevented;
}
const settle = () => new Promise(resolve => setImmediate(resolve));
(async () => {
  assert.equal(nodes['test-send'].disabled, true);
  draft.value = 'Hello';
  draft.listeners.input();
  assert.equal(nodes['test-send'].disabled, false);
  for (const modifiers of [{ shiftKey: true }, { isComposing: true }, { ctrlKey: true }, { metaKey: true }, { altKey: true }]) {
    assert.equal(key(modifiers), !modifiers.isComposing);
  }
  assert.equal(requests.length, 0);
  assert.equal(key(), true);
  assert.equal(requests.length, 1);
  assert.equal(requests[0].options.headers.Authorization, 'Bearer gateway-fixture');
  assert.equal(nodes['test-send'].disabled, true);
  key(); // Busy requests never double-submit.
  assert.equal(requests.length, 1);
  resolveFetch();
  await settle();
  assert.equal(draft.value, '');
  assert.equal(empty.hidden, true);
  const messages = nodes['test-log'].children;
  assert.equal(messages.length, 2);
  assert.equal(messages[0].className, 'test-msg test-msg-user');
  assert.equal(messages[1].children[1].textContent, '<script>plain text</script>');
  assert.equal(messages[1].scrolled, true);
  draft.value = 'Follow up';
  key();
  assert.equal(requests[1].body.messages.length, 3);
  const session = requests[0].options.headers['X-SparkRouter-Session-Id'];
  assert.match(session, /^[0-9a-f]{32}$/);
  assert.equal(requests[1].options.headers['X-SparkRouter-Session-Id'], session);
  fail = true;
  resolveFetch();
  await settle();
  assert.equal(draft.value, 'Follow up');
  assert.equal(nodes['test-error'].textContent, 'Provider unavailable');
  assert.equal(messages.length, 2);
  nodes['test-reset'].listeners.click();
  assert.equal(draft.value, '');
  assert.equal(empty.hidden, false);
  assert.equal(nodes['test-log'].children.length, 0);
  assert.equal(draft.focused, true);
  draft.value = 'After reset';
  key();
  assert.equal(requests[2].body.messages.length, 1);
  assert.notEqual(requests[2].options.headers['X-SparkRouter-Session-Id'], session);
  resolveFetch();
  await settle();
  fail = false;
  nodes['test-mode'].value = 'systemone';
  nodes['test-mode'].listeners.change();
  assert.equal(draft.hidden, true);
  assert.equal(nodes['test-evaluation'].hidden, false);
  assert.equal(nodes['test-send'].textContent, 'Evaluate');
  assert.equal(nodes['test-log'].children.length, 0);
  nodes['test-state'].value = 'not json';
  nodes['test-questions'].value = '{}';
  nodes['test-form'].listeners.submit({ preventDefault() {} });
  assert.equal(requests.length, 3);
  assert.match(nodes['test-error'].textContent, /valid JSON/);
  nodes['test-state'].value = JSON.stringify({ text: 'Help!' });
  nodes['test-questions'].value = JSON.stringify({ urgent: { type: 'noul', instructions: ['Urgent?'] } });
  nodes['test-state'].listeners.input();
  nodes['test-form'].listeners.submit({ preventDefault() {} });
  assert.equal(requests[3].url, '/test/systemone');
  assert.deepEqual(requests[3].body.state, { text: 'Help!' });
  assert.equal('messages' in requests[3].body, false);
  assert.equal(nodes['test-mode'].disabled, true);
  resolveFetch();
  await settle();
  const result = nodes['test-log'].children[0];
  assert.equal(result.children[0].textContent, 'SystemOne result');
  assert.match(result.children[1].textContent, /"noul": 0.95/);
  assert.match(result.children[1].textContent, /<script>safe<\/script>/);
  assert.equal(nodes['test-state'].value, JSON.stringify({ text: 'Help!' }));
  fail = true;
  nodes['test-form'].listeners.submit({ preventDefault() {} });
  resolveFetch();
  await settle();
  assert.equal(nodes['test-log'].children.length, 1);
  assert.equal(nodes['test-error'].textContent, 'Provider unavailable');
  nodes['test-reset'].listeners.click();
  assert.equal(nodes['test-log'].children.length, 0);
  assert.equal(nodes['test-state'].focused, true);
  nodes['test-mode'].value = 'chat';
  nodes['test-mode'].listeners.change();
  assert.equal(draft.hidden, false);
  assert.equal(nodes['test-evaluation'].hidden, true);
  windowListeners.pagehide();
  assert.equal(nodes['test-state'].value, '');
  assert.equal(nodes['test-questions'].value, '');
  assert.equal(nodes['test-key'].value, '');
  assert.equal(draft.value, '');
  console.log('Test playground checks passed: chat keyboard/context, SystemOne JSON/payload/results, mode switching, busy guard, safe text, retry, reset, secret cleanup.');
})().catch(error => { console.error(error); process.exitCode = 1; });
