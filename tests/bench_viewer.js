/* Real Chromium 10k/50k cluster benchmark, isolated HOME/profile and synthetic memory only.
 * CARGO_TARGET_DIR=... BRAIN_BENCH_OUTPUT=... node tests/bench_viewer.js
 * Browser/runtime errors fail the test. Never print the private window URL.
 */
const assert = require('node:assert/strict');
const fs = require('node:fs');
const os = require('node:os');
const path = require('node:path');
const {spawn, spawnSync} = require('node:child_process');
const delay = ms => new Promise(resolve => setTimeout(resolve, ms));
async function until(fn, description) {
  for(let i=0;i<400;i++) { const value=await fn(); if(value) return value; await delay(100); }
  throw new Error('timeout: '+description);
}
async function benchmark(count) {
  if(spawnSync('google-chrome',['--version']).status !== 0) {
    throw new Error('google-chrome required for benchmark');
  }
  const binary = process.env.BRAIN_TEST_BIN || path.resolve(process.env.CARGO_TARGET_DIR || 'target', 'debug/brain');
  assert(fs.existsSync(binary), 'build brain first or set BRAIN_TEST_BIN');
  const home=fs.mkdtempSync(path.join(os.tmpdir(),'brain-e4c-browser-'));
  const env={...process.env,HOME:home,XDG_CONFIG_HOME:path.join(home,'config'),XDG_CACHE_HOME:path.join(home,'cache'),BRAIN_OFFLINE:'true'};
  delete env.BRAIN_DATABASE;
  let server,chrome,ws;
  try {
    const initialized = spawnSync(binary, ['init'], {env, encoding:'utf8'});
    assert.equal(initialized.status, 0, initialized.stderr);
    const seeded = spawnSync('python3', ['-B', path.join(__dirname,'seed_viewer.py'), path.join(home,'.brain','brain.sqlite'), String(count)], {env,encoding:'utf8'});
    assert.equal(seeded.status, 0, seeded.stderr);
    server=spawn(binary,['ui','--no-open','--port','0'],{env,stdio:['ignore','pipe','pipe']});
    let output=''; server.stdout.on('data',chunk=>output+=chunk);
    const listening=await until(()=>{try{return JSON.parse(output);}catch{return null;}},'UI server');
    const owner=JSON.parse(fs.readFileSync(listening.owner_file,'utf8'));
    const profile=path.join(home,'chrome');
    chrome=spawn('google-chrome',['--headless=new','--no-sandbox','--disable-dev-shm-usage','--no-first-run','--no-default-browser-check','--disable-background-networking','--enable-unsafe-swiftshader','--use-angle=swiftshader','--remote-debugging-port=0','--user-data-dir='+profile,'about:blank'],{env,stdio:'ignore'});
    const devtools=path.join(profile,'DevToolsActivePort');
    await until(()=>fs.existsSync(devtools),'Chrome DevTools');
    const port=fs.readFileSync(devtools,'utf8').split('\n')[0];
    const tabs=await (await fetch('http://127.0.0.1:'+port+'/json/list')).json();
    ws=new WebSocket(tabs.find(tab=>tab.type==='page').webSocketDebuggerUrl);
    await new Promise((resolve,reject)=>{ws.addEventListener('open',resolve,{once:true});ws.addEventListener('error',reject,{once:true});});
    let seq=0; const pending=new Map(); const errors=[];
    ws.addEventListener('message',event=>{
      const message=JSON.parse(event.data);
      if(message.id) {const item=pending.get(message.id);pending.delete(message.id); if(message.error)item.reject(new Error(message.error.message));else item.resolve(message.result);}
      if(message.method==='Runtime.exceptionThrown') errors.push(message.params.exceptionDetails.text+': '+(message.params.exceptionDetails.exception?.description||''));
      if(message.method==='Log.entryAdded'&&message.params.entry.level==='error') errors.push(message.params.entry.text+' '+(message.params.entry.url ? new URL(message.params.entry.url).pathname : ''));
      if(message.method==='Runtime.consoleAPICalled'&&message.params.type==='error') errors.push(message.params.args.map(arg=>arg.value||arg.description).join(' '));
    });
    const call=(method,params={})=>new Promise((resolve,reject)=>{const id=++seq;pending.set(id,{resolve,reject});ws.send(JSON.stringify({id,method,params}));});
    await call('Runtime.enable'); await call('Log.enable'); await call('Page.enable');
    await call('Emulation.setFocusEmulationEnabled', {enabled:true});
    await call('Page.navigate',{url:'http://127.0.0.1:'+listening.port+'/graph/brain#k='+owner.token});
    const evaluate=async expression=>(await call('Runtime.evaluate',{expression,returnByValue:true})).result.value;
    await until(()=>evaluate('window.__GRAPHIFY_BRAIN_READY === true'),'viewer readiness');
    await until(()=>evaluate('window.__GRAPHIFY_BRAIN_FRAME_COUNT > 0'),'rendered frames');
    const readyMs = await evaluate('performance.now()');
    const initial = await evaluate('({nodes:DATA.nodes.length, total:DATA.stats.total_nodes, overview:DATA.clusterOverview, resources:performance.getEntriesByType("resource").filter(r=>r.name.includes("brain-data.json")||r.name.includes("brain-clusters.json")).map(r=>({path:new URL(r.name).pathname,bytes:r.decodedBodySize,ms:r.duration}))})');
    assert.equal(initial.total, count);
    assert.equal(initial.overview, true);
    assert(initial.nodes > 1 && initial.nodes <= count/100, 'communities remain distinct');
    async function sample(ms) {
      const before=await evaluate('window.__GRAPHIFY_BRAIN_FRAME_COUNT || 0');
      const started=Date.now(); await delay(ms);
      const after=await evaluate('({frames:window.__GRAPHIFY_BRAIN_FRAME_COUNT||0, mode:window.__GRAPHIFY_BRAIN_ENERGY_MODE})');
      return {fps: +((after.frames-before)*1000/(Date.now()-started)).toFixed(2),mode:after.mode};
    }
    // Real wall-clock intervals, no accelerated time and no forced render calls.
    await delay(400);
    const active=await sample(2500);
    await delay(3200);
    const idle=await sample(2500);
    await until(()=>evaluate('window.__GRAPHIFY_BRAIN_ENERGY_MODE === "sleep"'),'natural sleep');
    const sleep=await sample(2000);
    assert.equal(active.mode,'active'); assert.equal(idle.mode,'idle'); assert.equal(sleep.mode,'sleep');
    assert.equal(sleep.fps,0);
    // Drive the actual UI control across scene reloads, then collapse it.
    await evaluate('document.getElementById("clusterChooser").value="0"; document.getElementById("clusterChooser").dispatchEvent(new Event("change"))');
    await until(()=>evaluate('window.__GRAPHIFY_BRAIN_READY && !!window.__BRAIN_CLUSTER_STATE?.detail'),'expanded scene');
    const expanded=await evaluate('({nodes:DATA.nodes.length,id:window.__BRAIN_CLUSTER_STATE.detail.id,size:window.__BRAIN_CLUSTER_STATE.detail.size})');
    assert.equal(expanded.id,0); assert.equal(expanded.nodes, Math.min(500, expanded.size));
    await evaluate('document.querySelector("#clusterControls button").click()');
    await until(()=>evaluate('window.__GRAPHIFY_BRAIN_READY && DATA.clusterOverview === true'),'collapsed scene');
    assert.deepEqual(errors,[], 'browser console/runtime errors');
    const result={count,ready_ms:+readyMs.toFixed(1),visible_clusters:initial.nodes,resources:initial.resources,active,idle,sleep,expanded_nodes:expanded.nodes,console_errors:errors.length};
    console.log(JSON.stringify(result));
    return result;
  } finally {
    if(ws) ws.close();
    for(const child of [chrome,server]) if(child && child.exitCode===null) {
      const exited=new Promise(resolve=>child.once('exit',resolve)); child.kill('SIGTERM'); await exited;
    }
    fs.rmSync(home,{recursive:true,force:true});
  }
}
(async()=>{
  const results=[];
  for (const count of [10000,50000]) results.push(await benchmark(count));
  if(process.env.BRAIN_BENCH_OUTPUT) fs.writeFileSync(process.env.BRAIN_BENCH_OUTPUT, JSON.stringify({renderer:'Chromium headless, ANGLE SwiftShader, debug brain',results},null,2)+'\n');
})().catch(error=>{console.error('FAIL - viewer benchmark: '+error.message);process.exitCode=1;});
