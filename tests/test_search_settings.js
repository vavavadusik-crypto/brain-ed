/* Execute the production page functions with a small DOM: no duplicated draft logic. */
const fs = require('fs');
const vm = require('vm');
const assert = require('assert/strict');
const html = fs.readFileSync('brain-cli/assets/settings.html', 'utf8');
const elements = new Map();
class Element {
  constructor() { this.children = []; this.listeners = {}; this.dataset = {}; this.checked = false; this.value = '';  }
  addEventListener(name, cb) { this.listeners[name] = cb; }
  setAttribute() {}
  append(...children) { this.children.push(...children); }
  replaceChildren(...children) { this.children = children; }
  matches() { return false; }
}
let posts = 0, confirmed = false, prompts = [];
const sandbox = {
  window: {confirm: message => { prompts.push(message); return confirmed; }},
  document: {getElementById: id => {
    if (!elements.has(id)) elements.set(id, new Element());
    return elements.get(id);
  }, createElement: () => new Element(), documentElement: {dataset: {}}},
  localStorage: {getItem: () => null, setItem() {}},
  setInterval() {}, setTimeout() {}, URLSearchParams, location: {hash: '', search: ''},
  fetch: async (url, options) => {
    if (options?.method === 'POST') posts++;
    throw Error('synthetic transport failure');
  }, console,
};
vm.createContext(sandbox);
for (const script of html.matchAll(/<script[^>]*>([\s\S]*?)<\/script>/g)) {
  vm.runInContext(script[1], sandbox);
}
const run = code => vm.runInContext(code, sandbox);
run(`
state = {profile: 'balanced', overrides: {}, embedding_mode: 'always',
  profiles: [
    {name:'balanced', min_cosine:0.35,min_bm25:0.05,candidates_per_signal:200,graph_hops:1,budget_tokens:600},
    {name:'max', min_cosine:0.20,min_bm25:0.01,candidates_per_signal:1000,graph_hops:2,budget_tokens:2400}],
  baseline: {candidates_per_signal:200,budget_tokens:600},
  limits: {min_cosine:[0,0.99],min_bm25:[0,0.99],candidates_per_signal:[10,4000],graph_hops:[0,4],budget_tokens:[100,20000]}};
adopt(state, false);
window.__BrainMemoryDraft__.reset();
renderKnobs();
`);
const safety = sandbox.window.__BrainSearchSafety__;
assert.equal(safety.high('min_cosine', .99, false), .45);
assert.equal(safety.high('min_bm25', .99, false), .10);
assert.equal(safety.high('min_cosine', .99, true), .99);
assert(html.includes('выше = меньше результатов'));
const knobs = elements.get('knobs');
let cosine = knobs.children[3].children[2].children[0];
assert.equal(cosine.max, .45);
cosine.value = '.45';
cosine.listeners.input();
assert.equal(posts, 0, 'moving slider makes no request');
assert(run('draft.overrides.min_cosine === .45'));
assert(elements.get('search-warning').textContent.includes('min_cosine'));
assert(elements.get('search-summary').textContent.includes('0.35 (из профиля) → 0.45 (вручную)'));
(async () => {
  await run('save()');
  assert.equal(posts, 0, 'cancelling save sends no request');
  assert(prompts[0].includes('Было → стало'));
  confirmed = true;
  await run('save()');
  assert.equal(posts, 1, 'only confirmed Save posts');
  run(`draft.profile='max'; draft.overrides.min_cosine=.99; renderKnobs();`);
  cosine = knobs.children[3].children[2].children[0];
  assert.equal(cosine.disabled, true, 'loaded .99 cannot be edited at the safe slider');
  assert.equal(run('draft.overrides.min_cosine'), .99, 'loaded value was not silently changed');
  elements.get('advanced-search').checked = true;
  run('renderKnobs()');
  assert.equal(knobs.children[3].children[2].children[0].max, .99);
  run(`draft.embedding_mode='off'; window.__BrainMemoryDraft__.edit('include_archive',false); render = () => {};`);
  elements.get('reset').listeners.click();
  assert.equal(run('draft.profile'), 'balanced');
  assert.equal(run('Object.keys(draft.overrides).length'), 0);
  assert.equal(run('draft.embedding_mode'), 'off');
  assert.equal(run('window.__BrainMemoryDraft__.payload().include_archive'), false);
  assert.equal(posts, 1, 'reset is only a draft');
  assert(html.includes('Вернуть настройки по умолчанию'));
  run(`adopt(state, false); window.__BrainMemoryDraft__.edit('include_archive',false);
    adopt({...state, profile:'max', overrides:{min_cosine:.3}}, true);`);
  assert.equal(run('draft.profile'), 'max', 'memory-only draft adopts a foreign search update');
  assert.equal(run('draft.overrides.min_cosine'), .3);
  run(`draft.overrides.min_cosine=.4; adopt({...state, overrides:{min_cosine:.2}}, true);`);
  assert.equal(run('draft.overrides.min_cosine'), .4, 'user search draft survives refresh');
  console.log('search settings: thresholds, warning, draft, summary, confirmed save and scoped reset passed');
})().catch(error => { console.error(error); process.exitCode = 1; });
