/* Demonstrate that behavioral regressions reject individually broken fixes.
 * Run from the repository root: node tools/check-viewer-mutations.js
 * Only temporary synthetic copies are changed; production sources stay intact.
 */
const fs = require('node:fs');
const os = require('node:os');
const path = require('node:path');
const assert = require('node:assert/strict');
const {spawnSync} = require('node:child_process');
const root=path.resolve(__dirname,'..');
const appName='brain-cli/assets/viewer/app.js';
const pageName='brain-cli/assets/viewer/brain.html';
const originals=Object.fromEntries([appName,pageName].map(name=>[name,fs.readFileSync(path.join(root,name),'utf8')]));
const cases=[
  ['fetch receiver',pageName,'window.fetch.bind(window)','window.fetch.bind(window.fetch)'],
  ['fetch header merging',pageName,'init.headers = new Headers(init.headers || (input instanceof Request ? input.headers : undefined));\n        init.headers.set("X-Brain-Token", token);','init.headers = Object.assign({}, init.headers || { "X-Brain-Token": token });'],
  ['empty query stays local',appName,'if (!q) {',"if (!q && typeFilter.value === 'all') {"],
  ['empty API answer keeps graph',appName,'if (matches.length) {','if (true) {'],
  ['404 fallback',appName,'matches = DATA.nodes\n      .map','matches = []\n      .map'],
  ['nonzero small scores',appName,'Number(score).toPrecision(3)','Math.round(score)'],
  ['detail identity',appName,'detailsData.nodes.find(detail => detail[9] === node.id)','detailsData.nodes[index]'],
  ['total node count',appName,'DATA.stats.total_nodes ?? DATA.stats.nodes','DATA.stats.nodes'],
  ['sampling note',appName,"${DATA.stats.note || ''}",''],
  ['selected/hovered sleep',appName,'replayPlaying || dragging || nodeDragging || cameraTween','replayPlaying || dragging || nodeDragging || hoveredIndex >= 0 || selectedIndex >= 0 || cameraTween','energy'],
  ['5 second boundary',appName,'idleFor >= 5000','idleFor > 5000','energy'],
  ['30 second boundary',appName,'idleFor >= 30000','idleFor > 30000','energy'],
  ['animate sleep wiring',appName,"if (mode === 'sleep') {\n    enterSleep();","if (mode === 'sleep') {\n    /* broken sleep dispatch */",'energy'],
];
const temp=fs.mkdtempSync(path.join(os.tmpdir(),'e4c-viewer-mutations-'));
try {
  fs.mkdirSync(path.join(temp,'brain-cli/assets/viewer'),{recursive:true});
  for(const [label,file,before,after,suite='regressions'] of cases) {
    assert(originals[file].includes(before),'mutation anchor: '+label);
    for(const [name,source] of Object.entries(originals)) fs.writeFileSync(path.join(temp,name),name===file ? source.replace(before,after) : source);
    const run=spawnSync(process.execPath,[path.join(root,'tests/test_viewer_'+suite+'.js')],{env:{...process.env,VIEWER_SOURCE_ROOT:temp},encoding:'utf8'});
    assert.equal(run.status,1,'mutant survived: '+label+'\n'+run.stdout+run.stderr);
    assert(run.stderr.includes('FAIL -'),'mutant failed outside assertions: '+label);
    console.log('killed - '+label);
  }
  console.log('all 13 viewer JS mutations rejected');
} finally { fs.rmSync(temp,{recursive:true,force:true}); }
