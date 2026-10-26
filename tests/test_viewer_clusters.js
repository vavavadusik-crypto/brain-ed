/* Execute the shipped level-of-detail decisions in Node VM, no WebGL mock. */
const assert = require('node:assert/strict');
const fs = require('node:fs');
const vm = require('node:vm');
const path = require('node:path');
const source = fs.readFileSync(path.join(process.env.VIEWER_SOURCE_ROOT || path.join(__dirname, '..'), 'brain-cli/assets/viewer/app.js'), 'utf8');
function grab(name) {
  const start = source.indexOf('function ' + name + '(');
  assert(start >= 0, name + ' exists');
  let depth = 0;
  for (let i = source.indexOf('{', start); i < source.length; i++) {
    if (source[i] === '{') depth++;
    if (source[i] === '}' && --depth === 0) return source.slice(start, i + 1);
  }
  throw Error('unclosed ' + name);
}
const visits = [];
const overview = {generation: 'g-1', total_nodes: 10000, total_edges: 90,
  clusters: [{id: 0, size: 8000, label: '<script>alpha', lobe: 1}, {id: 1, size: 2000, label: 'beta', lobe: 2}],
  links: [{source: 0, target: 1, weight: 9}]};
const sandbox = {URLSearchParams, Math, Map, Number, String, clusterOverview: overview,
  location: {pathname: '/graph/brain', assign: url => visits.push(url)}, clusterDetail: null};
vm.createContext(sandbox);
vm.runInContext(['clusterScene', 'clusterLocation', 'expandCluster', 'collapseCluster', 'zoomCluster'].map(grab).join('\n'), sandbox);
const scene = sandbox.clusterScene({stats:{}}, overview);
assert.equal(scene.nodes.length, 2);
assert.equal(scene.stats.total_nodes, 10000);
assert.equal(scene.nodes.reduce((sum, row) => sum + row[16], 0), 10000);
assert(Math.abs(scene.nodes[0][9] / scene.nodes[1][9] - Math.cbrt(4)) < 1e-12, 'sphere volume represents membership');
assert.equal(scene.links.length, 1);
sandbox.DATA = {clusterOverview: true, nodes: [{id:'0'}, {id:'1'}]};
sandbox.expandCluster(1);
assert.equal(visits.pop(), '/graph/brain?view=clusters&cluster=1&generation=g-1');
sandbox.collapseCluster();
assert.equal(visits.pop(), '/graph/brain?view=clusters');
sandbox.zoomCluster(-100, 200, 1);
assert.equal(visits.pop(), '/graph/brain?view=clusters&cluster=1&generation=g-1');
sandbox.zoomCluster(-100, 400, 1);
sandbox.zoomCluster(-100, 200, -1);
assert.equal(visits.length, 0, 'no premature or untargeted expansion');
sandbox.DATA.clusterOverview = false;
sandbox.clusterDetail = {id:1};
sandbox.zoomCluster(100, 750, -1);
assert.equal(visits.pop(), '/graph/brain?view=clusters');
assert.equal(sandbox.clusterLocation(1, 500), '/graph/brain?view=clusters&cluster=1&generation=g-1&offset=500');
const many = {...overview, clusters: Array.from({length:50000}, (_,id)=>({id,size:1,label:'isolated',lobe:1})),links:[]};
assert.equal(sandbox.clusterScene({stats:{}}, many).nodes.length, 4000);
assert.equal(sandbox.clusterScene({stats:{}}, many, 12).nodes.length, 2000);
assert(source.includes('if (DATA.clusterOverview) { expandCluster(Number(DATA.nodes[index].id)); return; }'), 'click is wired');
assert(source.includes('zoomCluster(event.deltaY, newZ,'), 'wheel is wired');
console.log('ok - cluster overview, expansion, collapse, zoom and bounded pagination');
