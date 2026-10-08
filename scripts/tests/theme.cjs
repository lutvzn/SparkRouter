// Optional dependency-free tests; Node is not a runtime/build requirement.
const assert = require('node:assert/strict');
const fs = require('node:fs');
const vm = require('node:vm');
const source = fs.readFileSync('static/theme.js', 'utf8');
function setup(stored, blocked = false) {
  const label = {};
  const icon = { setAttribute() {} };
  const toggle = { listeners: {}, querySelector(s) { return s === '.btn-label' ? label : icon; }, addEventListener(e, f) { this.listeners[e] = f; }, setAttribute() {} };
  const select = { listeners: {}, disabled: true, addEventListener(e, f) { this.listeners[e] = f; } };
  const status = {};
  const document = { documentElement: { dataset: {} }, getElementById(id) { return { 'theme-toggle': toggle, 'settings-theme': select, 'theme-storage-status': status }[id]; }, addEventListener(e, f) { this.ready = f; } };
  let dark = false;
  let mediaChange;
  const window = { localStorage: { getItem() { if (blocked) throw Error(); return stored; }, setItem(k, v) { if (blocked) throw Error(); stored = v; } }, matchMedia() { return { matches: dark, addEventListener(e, f) { mediaChange = f; } }; } };
  vm.runInNewContext(source, { window, document });
  document.ready();
  return { select, toggle, label, status, document, stored: () => stored, osDark() { dark = true; mediaChange(); } };
}
for (const mode of ['system', 'light', 'dark']) {
  const page = setup(mode);
  assert.equal(page.select.value, mode);
  assert.equal(page.select.disabled, false);
  page.select.value = 'dark';
  page.select.listeners.change();
  assert.equal(page.document.documentElement.dataset.theme, 'dark');
  assert.equal(page.stored(), 'dark');
  assert.equal(page.label.textContent, 'Theme: Dark');
  page.toggle.listeners.click();
  assert.equal(page.select.value, 'light');
  page.select.value = 'system';
  page.select.listeners.change();
  page.osDark();
  assert.equal(page.document.documentElement.dataset.theme, 'system');
  assert.equal(page.label.textContent, 'Theme: Dark');
  assert.equal(setup(page.stored()).select.value, 'system');
}
const blocked = setup(null, true);
blocked.select.value = 'dark';
blocked.select.listeners.change();
assert.equal(blocked.document.documentElement.dataset.theme, 'dark');
assert.match(blocked.status.textContent, /storage unavailable/);
assert.equal(setup('invalid').select.value, 'system');
console.log('Theme settings tests passed.');
