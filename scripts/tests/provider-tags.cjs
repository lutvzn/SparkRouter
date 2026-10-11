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
const favoritesEmpty = { hidden: true };
const favorites = [tile("OpenAI", "text image"), tile("Anthropic", "text")];
const grids = [{ id: "provider-favorites-grid", querySelectorAll: () => favorites }, { id: "provider-grid", querySelectorAll: () => tiles }];
const document = {
  getElementById(id) { return id === "provider-search" ? input : id === "provider-favorites-empty" ? favoritesEmpty : empty; },
  querySelectorAll(selector) { return selector === "[data-provider-grid]" ? grids : filters; },
};
vm.runInNewContext(source.slice(start, end) + "filterProviderTiles();", { document });
const visible = () => tiles.map(t => !t.hidden);
assert.deepEqual(visible(), [true, true, true, true]);
filters[2].checked = true; filters[2].listeners.change();
assert.deepEqual(visible(), [false, false, true, false]);
assert.ok(favorites.every(t => t.hidden));
assert.equal(favoritesEmpty.hidden, false);
assert.match(favoritesEmpty.textContent, /match these filters/);
filters[1].checked = true; filters[1].listeners.change();
assert.deepEqual(visible(), [true, false, true, false]);
input.value = "open"; input.listeners.input();
assert.deepEqual(visible(), [true, false, false, false]);
assert.deepEqual(favorites.map(t => !t.hidden), [true, false]);
assert.equal(favoritesEmpty.hidden, true);
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
favorites.length = 0; input.listeners.input();
assert.equal(favoritesEmpty.hidden, false);
assert.match(favoritesEmpty.textContent, /No favorites yet/);
console.log("Provider tag checks passed: both grids, any-tag union, combined search, empty states, reset, and Enter.");
