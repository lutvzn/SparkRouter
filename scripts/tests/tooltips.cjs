// Optional development test: node scripts/tests/tooltips.cjs (no packages).
const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const vm = require('node:vm');

function events() {
  const listeners = new Map();
  return {
    addEventListener(name, handler) {
      if (!listeners.has(name)) listeners.set(name, []);
      listeners.get(name).push(handler);
    },
    emit(name, event = {}) {
      (listeners.get(name) || []).forEach((handler) => handler(event));
    },
  };
}
function help() {
  const classes = new Set();
  const button = { ...events(), focus() { this.focused = true; } };
  return {
    ...events(), button,
    classList: {
      add(name) { classes.add(name); }, remove(name) { classes.delete(name); },
      contains(name) { return classes.has(name); },
    },
    querySelector(selector) { assert.equal(selector, '.info-button'); return button; },
  };
}
const helps = [help(), help()];
const document = {
  ...events(), body: events(),
  getElementById() { return null; },
  querySelectorAll(selector) { return selector === '.field-help' ? helps : []; },
};
const source = fs.readFileSync(path.join(__dirname, '../../static/app.js'), 'utf8');
vm.runInNewContext(source, { document, navigator: {}, window: {} });
function escape() { document.emit('keydown', { key: 'Escape', target: {} }); }
escape();
assert(helps.every((item) => item.classList.contains('help-dismissed')),
  'Escape dismisses hover and focus help without moving focus');
helps[0].emit('pointerenter');
assert(!helps[0].classList.contains('help-dismissed'), 'next hover reopens help');
assert(helps[1].classList.contains('help-dismissed'), 'unrelated help remains dismissed');
helps[1].emit('focusin');
assert(!helps[1].classList.contains('help-dismissed'), 'next keyboard focus reopens help');
escape();
helps[0].button.emit('click', { currentTarget: helps[0].button });
assert(helps[0].button.focused, 'tapping focuses the info button, including Safari');
assert(!helps[0].classList.contains('help-dismissed'), 'tapping reopens dismissed help');
document.emit('keydown', { key: 'Enter', target: {} });
assert(!helps[0].classList.contains('help-dismissed'), 'other keys do not dismiss help');
console.log('Tooltip checks passed: Escape, hover/focus reopening, tap focus, and unrelated keys.');
