/* Execute shipped functions against synthetic browser state; no WebGL or user data. */
const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const vm = require('node:vm');
const root = process.env.VIEWER_SOURCE_ROOT || path.join(__dirname, '..');
const app = fs.readFileSync(path.join(root, 'brain-cli/assets/viewer/app.js'), 'utf8');
const page = fs.readFileSync(path.join(root, 'brain-cli/assets/viewer/brain.html'), 'utf8');
function grab(name) {
  const start = app.indexOf('function ' + name + '(');
  assert(start >= 0, name);
  const end = app.indexOf('\n}', start);
  return (app.slice(start - 6, start) === 'async ' ? 'async ' : '') + app.slice(start, end + 2);
}
let failed = 0;
async function test(name, fn) {
  try { await fn(); console.log('ok - ' + name); }
  catch (error) { failed++; console.error('FAIL - ' + name + ': ' + error.message); }
}
(async () => {
  await test('fetch binds window and merges object, pairs, Headers and Request headers', async () => {
    const calls = [];
    const window = { fetch(input, init) { assert.equal(this, window, 'native fetch receiver'); calls.push(init); } };
    const token = 'a'.repeat(64);
    const sandbox = {window, Headers, Request, location: {hash: '#k=' + token, pathname:'/graph/brain',search:''}, history:{replaceState(){}},sessionStorage:{setItem(){},getItem(){return token;}}};
    vm.runInNewContext(page.match(/<script>([\s\S]*?)<\/script>/)[1], sandbox);
    for (const headers of [{Accept:'application/json'}, [['Accept','application/json']], new Headers({Accept:'application/json', 'X-Brain-Token':'wrong'})]) {
      await window.fetch('/api/search', {headers,cache:'no-cache'});
      const sent = new Headers(calls.at(-1).headers);
      assert.equal(sent.get('x-brain-token'), token);
      assert.equal(sent.get('accept'), 'application/json');
      assert.equal(calls.at(-1).cache, 'no-cache');
      assert.notEqual(calls.at(-1).headers, headers, 'do not mutate caller');
    }
    await window.fetch(new Request('http://localhost/api/node', {headers:{Accept:'text/plain'}}));
    assert.equal(new Headers(calls.at(-1).headers).get('accept'), 'text/plain');
    assert.equal(new Headers(calls.at(-1).headers).get('x-brain-token'), token);
  });
  function searchState(q, type, response) {
    const nodes = [{id:'a',label:'zebra',type:'memory',community:0,brain_lobe:'memory',extended_search:'zebra'}, {id:'b',label:'code',type:'code',community:1,brain_lobe:'code',extended_search:'code'}];
    const state = {search:{value:q},searchRequest:0, DATA:{nodes},nodeById:new Map(nodes.map((node,index)=>[node.id,{node,index}])),typeFilter:{value:type},communityFilter:{value:'all'},lobeFilter:{value:'all'},sortMode:{value:'label'},visibleSet:new Set(),refreshVisualState(){},renderLabels(){},results:{style:{}},scoreNode:()=>0.0123,renderSearchResults(matches){state.matches=matches;},fetch:async()=>{state.calls++;return response;},calls:0};
    vm.createContext(state);
    vm.runInContext(['passesFilters','sortMatches','updateSearch'].map(grab).join('\n'),state);
    return state;
  }
  await test('empty q with a filter stays local and retains filtered nodes', async () => {
    const s=searchState('  ','memory',{ok:true,json:async()=>[]}); await s.updateSearch();
    assert.equal(s.calls,0); assert.deepEqual([...s.visibleSet],[0]);
    assert.equal(s.passesFilters(s.DATA.nodes[1],1),false);
  });
  await test('200 [] retains the filtered graph and clears result cards', async () => {
    const s=searchState('unknown','all',{ok:true,json:async()=>[]}); await s.updateSearch();
    assert.deepEqual([...s.visibleSet],[0,1]); assert.equal(s.matches.length,0);
  });
  await test('404 search falls back to local matching and respects filters', async () => {
    const s=searchState('zeb','memory',{ok:false,status:404}); await s.updateSearch();
    assert.deepEqual([...s.visibleSet],[0]); assert.equal(s.matches[0].node.id,'a');
  });
  await test('small search scores remain nonzero in result cards', () => {
    const s={results:{style:{},querySelectorAll:()=>[]},activeSearchResult:-1,esc:String};
    vm.createContext(s); vm.runInContext(grab('renderSearchResults'),s);
    s.renderSearchResults([{node:{id:'a',label:'zebra'},index:0,score:0.0000123}]);
    const score=s.results.innerHTML.match(/class="score">([^<]+)/)[1];
    assert(Number(score)>0,score);
  });
  await test('details join by id even after reordering and omit missing ids', async () => {
    // Execute the actual lookup portion of the inspector before its HTML rendering.
    const start=app.indexOf('async function renderInspector(');
    const end=app.indexOf('  inspector.innerHTML =',start);
    const s={DATA:{nodes:[{id:'a'},{id:'b'}]},selectedIndex:0,loadDetails:async()=>({nodes:[['B',null,null,null,null,0,false,0,0,'b'],['A',null,null,null,null,0,false,0,0,'a']]})};
    vm.createContext(s); vm.runInContext(app.slice(start,end)+'return details;\n}',s);
    assert.equal((await s.renderInspector(0)).source_location, 'A');
    s.selectedIndex = 1;
    s.loadDetails = async () => ({nodes:[['A',null,null,null,null,0,false,0,0,'a']]});
    assert.equal((await s.renderInspector(1)).source_location, undefined);
  });
  await test('stats display total_nodes and sampling note', () => {
    const start=app.indexOf("const statsPrefix =");
    assert(start>=0);
    const end=app.indexOf(';',app.indexOf('statsElement.textContent',start));
    const element={}; const s={statsElement:element,document:{getElementById:()=>element},DATA:{stats:{nodes:4000,total_nodes:4001,links:2,active_nodes:1,note:'показаны 4000 самых связанных из 4001'},lobes:[{count:1}]}};
    vm.runInNewContext(app.slice(start,end+1),s);
    assert(element.textContent.startsWith('4001 узлов')); assert(element.textContent.includes(s.DATA.stats.note));
  });
  if(failed) process.exitCode=1; else console.log('all viewer regression checks passed');
})();
