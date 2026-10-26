/* Real Chromium smoke, isolated HOME/profile and synthetic memory only.
 * CARGO_TARGET_DIR=... node tests/test_viewer_browser.js
 * Browser/runtime errors fail the test. Never print the private window URL.
 */
const assert = require('node:assert/strict');
const fs = require('node:fs');
const os = require('node:os');
const path = require('node:path');
const {spawn, spawnSync} = require('node:child_process');
const delay = ms => new Promise(resolve => setTimeout(resolve, ms));
async function until(fn, description) {
  for(let i=0;i<200;i++) { const value=await fn(); if(value) return value; await delay(100); }
  throw new Error('timeout: '+description);
}
(async () => {
  if(spawnSync('google-chrome',['--version']).status !== 0) {
    console.log('SKIP - google-chrome unavailable'); return;
  }
  const binary = process.env.BRAIN_TEST_BIN || path.resolve(process.env.CARGO_TARGET_DIR || 'target', 'debug/brain');
  assert(fs.existsSync(binary), 'build brain first or set BRAIN_TEST_BIN');
  const home=fs.mkdtempSync(path.join(os.tmpdir(),'brain-e4c-browser-'));
  const env={...process.env,HOME:home,XDG_CONFIG_HOME:path.join(home,'config'),XDG_CACHE_HOME:path.join(home,'cache'),BRAIN_OFFLINE:'true'};
  delete env.BRAIN_DATABASE;
  let server,chrome,ws;
  try {
    for(const args of [['init'],['--offline','remember','synthetic zebra viewer smoke']]) {
      const result=spawnSync(binary,args,{env,encoding:'utf8'});
      assert.equal(result.status,0,'synthetic fixture command failed');
    }
    fs.mkdirSync(path.join(home,'.cursor'));
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
    await call('Page.navigate',{url:'http://127.0.0.1:'+listening.port+'/graph/brain#k='+owner.token});
    const evaluate=async expression=>(await call('Runtime.evaluate',{expression,returnByValue:true})).result.value;
    await until(()=>evaluate('window.__GRAPHIFY_BRAIN_READY === true'),'viewer readiness');
    await until(()=>evaluate('window.__GRAPHIFY_BRAIN_FRAME_COUNT > 0'),'rendered frames');
    await delay(500);
    const observed=await evaluate('({ready:window.__GRAPHIFY_BRAIN_READY,nodes:window.__GRAPHIFY_BRAIN_DATA.nodes.length,frames:window.__GRAPHIFY_BRAIN_FRAME_COUNT,loading:document.getElementById("loading").style.display,title:document.title})');
    assert.equal(observed.nodes,1); assert.equal(observed.loading,'none');
    assert.equal(observed.title,'New Generation Brain');
    assert.deepEqual(errors,[], 'browser console/runtime errors');
    console.log('ok - headless Chrome loaded /graph/brain: '+JSON.stringify(observed)+'; console/runtime errors=0');
    await call('Page.navigate',{url:'http://127.0.0.1:'+listening.port+'/#k='+owner.token});
    await until(()=>evaluate('!!document.querySelector("button[data-agent=cursor]")'),'settings agents');
    await until(()=>evaluate('!!document.getElementById("privacy-apply")'),'privacy controls');
    assert.equal(await evaluate('document.querySelectorAll("#privacy-controls tbody tr").length'),3);
    await evaluate('document.querySelector("button[data-agent=cursor]").click()');
    await until(()=>evaluate('!!document.getElementById("agents-confirm") && !document.getElementById("agents-confirm").disabled'),'agent preview');
    assert(!fs.existsSync(path.join(home,'.cursor/mcp_config.json')),'preview must not write config');
    assert(!fs.existsSync(path.join(home,'.brain/install-manifest.json')),'preview must not write manifest');
    assert.equal(await evaluate('document.querySelector("#agents-preview pre").textContent.includes("mcpServers")'),true,'connect preview contains MCP diff');
    await evaluate('document.getElementById("agents-confirm").click()');
    await until(()=>fs.existsSync(path.join(home,'.brain/install-manifest.json')),'confirmed connect');
    await until(()=>evaluate('document.querySelector("button[data-agent=cursor]").dataset.action === "disconnect"'),'connected state');
    await evaluate('document.querySelector("button[data-agent=cursor]").click()');
    await until(()=>evaluate('!!document.getElementById("agents-confirm") && !document.getElementById("agents-confirm").disabled && document.getElementById("agents-confirm").textContent.includes("Отключить")'),'disconnect preview');
    assert(fs.existsSync(path.join(home,'.cursor/mcp_config.json')),'disconnect preview keeps config');
    assert.equal(await evaluate('Array.from(document.querySelectorAll("#agents-preview pre")).some(pre => pre.textContent.split(String.fromCharCode(10)).some(line => line.startsWith("-") && !line.startsWith("---")))'),true,'disconnect preview contains deletion diff');
    await evaluate('document.getElementById("agents-confirm").click()');
    await until(()=>!fs.existsSync(path.join(home,'.cursor/mcp_config.json')),'confirmed disconnect');
    await until(()=>evaluate('document.querySelector("button[data-agent=cursor]").dataset.action === "connect"'),'disconnected state');
    const collector = path.join(home,'.brain/archive/collector.json');
    await evaluate('document.getElementById("privacy-enabled").click(); document.getElementById("privacy-apply").click()');
    assert(!fs.existsSync(collector),'privacy first click must not write');
    await evaluate('document.getElementById("privacy-apply").click()');
    await until(()=>fs.existsSync(collector),'confirmed privacy patch');
    assert.equal(JSON.parse(fs.readFileSync(collector,'utf8')).enabled,true,'privacy consent persisted');
    assert.deepEqual(errors,[], 'settings console/runtime errors');
    console.log('ok - settings panels: agent connect/disconnect diffs and explicit confirmation, privacy confirmation, three client rows; console/runtime errors=0');
  } finally {
    if(ws) ws.close();
    for(const child of [chrome,server]) if(child && child.exitCode===null) {
      const exited=new Promise(resolve=>child.once('exit',resolve)); child.kill('SIGTERM'); await exited;
    }
    fs.rmSync(home,{recursive:true,force:true});
  }
})().catch(error=>{console.error('FAIL - browser smoke: '+error.message);process.exitCode=1;});
