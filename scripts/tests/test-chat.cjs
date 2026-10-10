// Dependency-free interaction checks; this is a DOM mock, not a visual browser test.
const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const vm = require('node:vm');

class Element {
  constructor() {
    this._value = '';
    this.files = [];
    this.hidden = false;
    this.children = [];
    this.listeners = {};
    this.style = {};
    this.scrollHeight = 64;
    this.textContent = '';
  }
  get value() { return this._value; }
  set value(value) { this._value = value; if (!value) this.files = []; }
  pause() { this.paused = true; }
  load() { this.loaded = true; }
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
  'test-mode', 'test-evaluation', 'test-state', 'test-questions',
  'test-media', 'test-media-hint', 'test-upload-controls', 'test-file', 'test-mask-control',
  'test-mask', 'test-voice-control', 'test-voice', 'test-media-options'];
const nodes = Object.fromEntries(ids.map(id => [id, new Element()]));
const empty = new Element();
const attemptsEmpty = new Element();
const windowListeners = {};
const requests = [];
let fail = false;
let resolveFetch;
let mediaMime = 'application/json';
let mediaJson = { data: [{ url: 'https://example.com/output' }] };
let interrupted = false;
const createdUrls = [];
const revokedUrls = [];
class MediaURL extends URL {
  static createObjectURL(blob) { const url = 'blob:fixture-' + createdUrls.length; createdUrls.push({ url, blob }); return url; }
  static revokeObjectURL(url) { revokedUrls.push(url); }
}
const { File } = require('node:buffer');
nodes['test-mode'].value = 'chat';
nodes['test-combo'].value = 'coding';
nodes['test-combo'].options = [['coding','text'],['other','text'],['decisions','decision'],['images','image'],['videos','video'],['music','music'],['speech','tts'],['audio','stt']].map(([value,category]) => ({ value, dataset:{category}, hidden:false, disabled:false }));
nodes['test-csrf'].value = 'csrf-fixture';
nodes['test-key'].value = 'gateway-fixture';
vm.runInNewContext(fs.readFileSync(path.join(__dirname, '../../static/test-chat.js'), 'utf8'), {
  document: {
    getElementById: id => nodes[id],
    querySelector: () => attemptsEmpty,
    createElement: () => new Element(),
  },
  window: { addEventListener: (name, fn) => { windowListeners[name] = fn; } },
  AbortController, URL: MediaURL, Blob, FormData, atob, TextDecoder, Uint8Array,
  crypto: require('node:crypto').webcrypto,
  fetch: (url, options) => {
    requests.push({ url, options, body: typeof options.body === 'string' ? JSON.parse(options.body) : options.body });
    return new Promise(resolve => {
      resolveFetch = () => resolve({ ok: !fail, status: fail ? 502 : 200,
        headers: { get: name => name === 'content-type' ? mediaMime : name === 'x-sparkrouter-attempts' ? Buffer.from(JSON.stringify({ attempts: [{ provider_name: 'Provider ✦', model: 'media', position: 1, outcome: 'selected', latency_ms: 3 }], truncated: true })).toString('base64') : null },
        blob: async () => { if (interrupted) throw new Error('interrupted'); return new Blob(['audio bytes'], { type: mediaMime }); },
        text: async () => '<script>transcript stays text</script>',
        json: async () => fail
          ? { error: { message: 'Provider unavailable' }, attempts: [] }
          : url.startsWith('/test/media') ? mediaJson : { completion: url === '/test/systemone'
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
  fail = false;
  function mode(value) { nodes['test-mode'].value = value; nodes['test-mode'].listeners.change(); }
  function submit() { nodes['test-form'].listeners.submit({ preventDefault() {} }); }
  mode('image');
  draft.value = 'A tiny image';
  nodes['test-media-options'].value = '[]';
  const beforeInvalid = requests.length;
  submit();
  assert.equal(requests.length, beforeInvalid);
  assert.match(nodes['test-error'].textContent, /JSON object/);
  nodes['test-media-options'].value = '{"response_format":"b64_json","model":"unauthorized","_csrf":"leak"}';
  mediaJson = { data: [{ b64_json: 'iVBORw0KGgo=' }] };
  submit();
  assert.equal(requests.at(-1).url, '/test/media/image');
  assert.equal(requests.at(-1).body.model, 'combo/images');
  assert.equal(requests.at(-1).body.prompt, 'A tiny image');
  assert.equal('_csrf' in requests.at(-1).body, false);
  assert.equal(requests.at(-1).options.headers['X-SparkRouter-CSRF'], 'csrf-fixture');
  resolveFetch(); await settle();
  assert.equal(draft.value, 'A tiny image');
  assert.equal(nodes['test-log'].children[0].children[2].src, createdUrls[0].url);
  assert.equal(createdUrls[0].blob.type, 'image/png');
  assert.match(nodes['test-log'].children[0].children[1].textContent, /base64 image/);
  assert.equal(nodes['test-attempt-list'].children[0].children[1].textContent, 'Provider ✦');
  assert.match(attemptsEmpty.textContent, /Only the first attempts/);
  const firstImageUrl = createdUrls[0].url;
  mode('video');
  assert.ok(revokedUrls.includes(firstImageUrl));
  mediaJson = { data: [{ url: 'javascript:alert(1)' }, { url: 'https://user:pass@example.com' }, { url: '/relative' }, { url: 'https://example.com/video.mp4' }] };
  draft.value = 'A short video';
  submit();
  assert.equal(requests.at(-1).url, '/test/media/video');
  resolveFetch(); await settle();
  const video = nodes['test-log'].children[0];
  assert.equal(video.children.length, 3);
  assert.equal(video.children[2].href, 'https://example.com/video.mp4');
  assert.equal(video.children[2].rel, 'noopener noreferrer');
  assert.equal(video.children[2].referrerPolicy, 'no-referrer');
  assert.equal(video.children.some(node => node.src), false); // No automatic remote fetch.
  mode('music');
  draft.value = '';
  nodes['test-media-options'].value = '{"lyrics":"A song","output_format":"mp3"}';
  mediaMime = 'audio/mpeg';
  submit();
  assert.equal(requests.at(-1).url, '/test/media/music');
  assert.equal(requests.at(-1).body.lyrics, 'A song');
  resolveFetch(); await settle();
  const musicPlayer = nodes['test-log'].children[0].children[1];
  assert.equal(musicPlayer.controls, true);
  assert.equal(musicPlayer.preload, 'metadata');
  assert.equal(nodes['test-log'].children[0].children[2].download, 'audio.mp3');
  mode('speech');
  assert.equal(musicPlayer.paused, true);
  assert.equal(musicPlayer.loaded, true);
  draft.value = 'Hello audio';
  nodes['test-voice'].value = 'voice-fixture';
  submit();
  assert.equal(requests.at(-1).body.input, 'Hello audio');
  assert.equal(requests.at(-1).body.voice, 'voice-fixture');
  assert.equal('messages' in requests.at(-1).body, false);
  interrupted = true;
  resolveFetch(); await settle();
  assert.equal(nodes['test-log'].children.length, 0);
  assert.match(nodes['test-error'].textContent, /already have been billed/);
  assert.equal(nodes['test-send'].disabled, false);
  interrupted = false;
  mode('transcription');
  mediaMime = 'text/plain';
  assert.equal(draft.hidden, true);
  submit(); // Missing upload is not submitted.
  assert.equal(requests.at(-1).url, '/test/media/speech');
  nodes['test-file'].files = [new File(['RIFF-fixture'], 'audio.wav', { type: 'audio/wav' })];
  nodes['test-media-options'].value = '{"language":"en","timestamp_granularities[]":["word","segment"]}';
  submit();
  const transcription = requests.at(-1);
  assert.equal(transcription.url, '/test/media-upload/transcription');
  assert.equal('Content-Type' in transcription.options.headers, false); // Browser supplies boundary.
  assert.equal(transcription.body.get('file').name, 'audio.wav');
  assert.equal(transcription.body.get('model'), 'combo/audio');
  assert.deepEqual(transcription.body.getAll('timestamp_granularities[]'), ['word', 'segment']);
  resolveFetch(); await settle();
  assert.equal(nodes['test-log'].children[0].children[1].textContent, '<script>transcript stays text</script>');
  mode('translation');
  nodes['test-file'].files = [new File(['audio'], 'audio.mp3')];
  submit();
  assert.equal(requests.at(-1).url, '/test/media-upload/translation');
  resolveFetch(); await settle();
  mode('image-edit');
  draft.value = 'Edit this image';
  nodes['test-file'].files = [new File(['image'], 'image.png')];
  nodes['test-mask'].files = [new File(['mask'], 'mask.png')];
  mediaMime = 'application/json';
  mediaJson = { data: [{ b64_json: 'iVBORw0KGgo=' }] };
  submit();
  assert.equal(requests.at(-1).body.get('image').name, 'image.png');
  assert.equal(requests.at(-1).body.get('mask').name, 'mask.png');
  assert.equal(requests.at(-1).body.get('prompt'), 'Edit this image');
  resolveFetch(); await settle();
  nodes['test-combo'].listeners.change();
  assert.equal(nodes['test-log'].children.length, 0);
  assert.equal(revokedUrls.length, createdUrls.length);
  mode('speech');
  draft.value = 'Pending audio';
  mediaMime = 'audio/mpeg';
  submit();
  const urlCount = createdUrls.length;
  windowListeners.pagehide();
  assert.equal(nodes['test-voice'].value, '');
  assert.equal(nodes['test-media-options'].value, '{}');
  assert.equal(nodes['test-file'].files.length, 0);
  assert.equal(nodes['test-mask'].files.length, 0);
  resolveFetch(); await settle();
  assert.equal(nodes['test-log'].children.length, 0);
  assert.equal(createdUrls.length, urlCount);
  mode('image');
  assert.equal(nodes['test-combo'].value, 'images');
  assert.deepEqual(nodes['test-combo'].options.filter(option => !option.hidden).map(option => option.value), ['images']);
  mode('systemone');
  assert.equal(nodes['test-combo'].value, 'decisions');
  mode('translation');
  assert.equal(nodes['test-combo'].value, 'audio');
  nodes['test-combo'].options = nodes['test-combo'].options.filter(option => option.dataset.category !== 'video');
  mode('video');
  draft.value = 'Cannot submit without matching combo';
  draft.listeners.input();
  assert.equal(nodes['test-combo'].value, '');
  assert.equal(nodes['test-send'].disabled, true);
  const noComboCount = requests.length;
  submit();
  assert.equal(requests.length, noComboCount);
  console.log('Test playground checks passed: category filtering/empty modes,  chat/SystemOne and all media modes, JSON validation, uploads/masks, native options, Unicode traces, safe previews/links, streaming errors, URL cleanup and stale-response/secret cleanup.');
})().catch(error => { console.error(error); process.exitCode = 1; });
