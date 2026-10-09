// Dependency-free DOM checks for provider name + any-tag filtering.
const assert = require("node:assert/strict");
const fs = require("node:fs");
const vm = require("node:vm");
const source = fs.readFileSync("static/app.js", "utf8");
const start = source.indexOf("function filterProviderTiles()");
const end = source.indexOf("filterProviderTiles();", start);
const input = { value: "", listeners: {}, addEventListener(name, callback) { this.listeners[name] = callback; } };
const filters = ["text", "image", "decision", "music"].map(value => ({ value, checked: false, listeners: {}, addEventListener(name, callback) { this.listeners[name] = callback; } }));
const tile = (name, tags) => ({ hidden: false, querySelector: () => ({ textContent: name }), getAttribute: () => tags });
const tiles = [tile("OpenAI", "text image"), tile("Anthropic", "text"), tile("TypeSafe AI", "decision"), tile("Custom provider", null)];
const empty = { hidden: true };
const document = {
  getElementById(id) { return id === "provider-search" ? input : id === "provider-grid" ? { querySelectorAll: () => tiles } : empty; },
  querySelectorAll: () => filters,
};
vm.runInNewContext(source.slice(start, end) + "filterProviderTiles();", { document });
const visible = () => tiles.map(t => !t.hidden);
assert.deepEqual(visible(), [true, true, true, true]);
filters[2].checked = true; filters[2].listeners.change();
assert.deepEqual(visible(), [false, false, true, false]);
filters[1].checked = true; filters[1].listeners.change();
assert.deepEqual(visible(), [true, false, true, false]);
input.value = "open"; input.listeners.input();
assert.deepEqual(visible(), [true, false, false, false]);
input.value = "<script>"; input.listeners.input();
assert.deepEqual(visible(), [false, false, false, false]);
assert.equal(empty.hidden, false);
assert.ok(empty.textContent.includes("<script>"));
input.value = ""; filters.forEach(f => { f.checked = false; }); input.listeners.input();
assert.deepEqual(visible(), [true, true, true, true]);
filters[3].checked = true; filters[3].listeners.change();
assert.equal(empty.hidden, false);
let prevented = false; input.listeners.keydown({ key: "Enter", preventDefault() { prevented = true; } });
assert.equal(prevented, true);
console.log("Provider tag checks passed: defaults, any-tag union, combined search, empty state, reset, and Enter.");
