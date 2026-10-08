// Isolated loopback UI proof. No host, file socket, OS input or production UI.
import { spawn, spawnSync } from "node:child_process";
import { randomUUID } from "node:crypto";
import { cpSync, mkdirSync, writeFileSync } from "node:fs";
import { createServer } from "node:net";
import { resolve } from "node:path";
import assert from "node:assert/strict";
import { startDirectLabRelay } from "./stream-lab-direct-relay.mjs";
import { CdpPage, buildReceiverChromeArgs } from "./stream-lab-live.mjs";
import { findChrome } from "./stream-lab-runtime.mjs";

const root=resolve(import.meta.dirname,"../../..");
const dir=resolve(root,"artifacts/file-drop-preview",randomUUID());
const web=resolve(dir,"fixture");mkdirSync(web,{recursive:true});
for(const name of ["file-ui.js","file-client.js","file-transfer.js","bubbleplay.css","fonts"])cpSync(resolve(root,"worker/public",name),resolve(web,name),{recursive:true});
writeFileSync(resolve(web,"fixture.html"),`<!doctype html><meta charset="utf-8"><title>Local file-drop preview fixture</title>
<link rel="stylesheet" href="/bubbleplay.css"><style>
body{margin:0;overflow:hidden}#canvas{height:100vh;background:#20232a;padding:48px 32px;box-sizing:border-box}
.fixture-label{color:#c6baac;font-size:13px;margin:0 0 26px}.workspace{width:65%;max-width:820px;background:#fafafa;color:#282a2f;height:570px;display:grid;grid-template-rows:48px 1fr;border:1px solid #999da6}.window-head{padding:14px 20px;border-bottom:1px solid #dcd4c8;background:#f3efe7;font-weight:650}.window-body{display:grid;grid-template-columns:160px 1fr}.folder-nav{padding:24px 20px;background:#f3efe7;line-height:2.4}.folder-content{padding:24px}.folder-content h1{font-size:24px;margin:0 0 24px}.file-line{padding:13px 0;border-bottom:1px solid #dcd4c8;display:flex;justify-content:space-between;font-size:14px}.file-line small{color:#625c55}.canvas-note{color:#c6baac;position:absolute;right:40px;bottom:36px;font-size:13px}
@media(max-width:600px){#canvas{padding:24px 16px}.workspace{width:100%;height:550px}.window-body{grid-template-columns:90px 1fr}.folder-nav{font-size:12px;padding:20px 10px}.folder-content{padding:20px 12px}.folder-content h1{font-size:20px}.file-line{font-size:12px}.canvas-note{right:16px;bottom:24px}}
</style><main id="canvas"><p class="fixture-label">Local visual fixture · synthetic desktop content</p><section class="workspace"><header class="window-head">Project files</header><div class="window-body"><nav class="folder-nav">Home<br>Desktop<br>Documents<br>Downloads</nav><section class="folder-content"><h1>Shared work</h1><div class="file-line">Notes.txt <small>Text document</small></div><div class="file-line">Reference.png <small>PNG image</small></div><div class="file-line">Drafts <small>Folder</small></div></section></div></section><p class="canvas-note">The stream stays visible while choosing a spot.</p></main>
<section id="file-transfer" hidden><div data-file-offer><span data-file-offer-label></span><button data-file-offer-download></button><button data-file-offer-dismiss></button></div><ul data-file-list></ul></section>
<div id="file-drop-veil" hidden aria-hidden="true"><div><strong></strong><span></span></div></div>
<script type="module">
import {installFileTransferUi} from '/file-ui.js';
window.fileReads=0;window.calls=[];window.route='drop';window.cancelled=[];
const listeners=[];
const client={offer:null,transfers:[],addEventListener(type,fn){listeners.push(fn)},upload(files,purpose){calls.push([files,purpose]);const ids=files.map(file=>{const id=this.transfers.length+1;this.transfers.push({id,name:file.name,size:file.size,bytes:0,direction:'upload',purpose,state:'queued'});return id});listeners.forEach(fn=>fn());return ids},cancel(id){cancelled.push(id);this.transfers.forEach(row=>row.state='cancelled');listeners.forEach(fn=>fn())},dismiss(id){this.transfers=this.transfers.filter(row=>row.id!==id);listeners.forEach(fn=>fn())},rejectUpload(...args){calls.push(args)}};
window.ui=installFileTransferUi({client,root:document.querySelector('#file-transfer'),veil:document.querySelector('#file-drop-veil'),resolveDrop:e=>route==='blocked'?{error:'drop-stale-view'}:route==='save'?{purpose:'save'}:{purpose:'drop',drop:{x:e.clientX,y:e.clientY}}});ui.setEnabled(true);
window.preview=(x,y,nextRoute='drop',count=2)=>{route=nextRoute;const e=new DragEvent(document.querySelector('#file-drop-veil').hidden?'dragenter':'dragover',{bubbles:true,cancelable:true,clientX:x,clientY:y});const transfer={types:['Files'],items:[...Array(count).fill(null).map(()=>({kind:'file',getAsFile(){fileReads++;throw Error('file read during preview')}})),{kind:'string',getAsFile(){throw Error('string read')}}]};Object.defineProperty(transfer,'files',{get(){throw Error('files read during preview')}});Object.defineProperty(e,'dataTransfer',{value:transfer});document.querySelector('#canvas').dispatchEvent(e);return transfer.dropEffect};
window.snapshot=()=>{const veil=document.querySelector('#file-drop-veil'),badge=veil.querySelector('div'),marker=veil.querySelector('i');const rect=e=>{const r=e.getBoundingClientRect();return{x:r.x,y:r.y,width:r.width,height:r.height}};return{mode:veil.dataset.mode,state:veil.dataset.state,display:getComputedStyle(veil).display,background:getComputedStyle(veil).backgroundColor,border:getComputedStyle(veil).borderWidth,title:veil.querySelector('strong').textContent,detail:veil.querySelector('span').textContent,badge:rect(badge),marker:rect(marker),pointerEvents:getComputedStyle(badge).pointerEvents,transition:getComputedStyle(marker).transitionDuration,fileReads,calls:calls.length}};
window.sendLarge=(x,y)=>{route='drop';ui.setEnabled(false);client.transfers=[];ui.setEnabled(true);const e=new DragEvent('drop',{bubbles:true,cancelable:true,clientX:x,clientY:y});Object.defineProperty(e,'dataTransfer',{value:{types:['Files'],items:[{kind:'file',getAsFile(){fileReads++;return{name:'Synthetic large.bin',size:10*1024*1024}}},{kind:'file',getAsFile(){fileReads++;return{name:'Empty.txt',size:0}}}]}});document.querySelector('#canvas').dispatchEvent(e)};
window.transferState=(state)=>{client.transfers.forEach(row=>{row.state=state==='sending'?'active':state==='attaching'?'verifying':state;row.bytes=state==='sending'?row.size/2:row.size;row.error=state==='failed'?'drop-rejected':state==='reset'?'disconnected':null;if(state==='reset')row.state='failed'});listeners.forEach(fn=>fn())};
window.progressSnapshot=()=>{const el=document.querySelector('#file-drop-progress'),button=el.querySelector('button'),r=el.getBoundingClientRect();return{state:el.dataset.state,display:getComputedStyle(el).display,title:el.querySelector('strong').textContent,detail:el.querySelector('span').textContent,action:button.textContent,label:button.getAttribute('aria-label'),badge:{x:r.x,y:r.y,width:r.width,height:r.height},pointerEvents:getComputedStyle(el).pointerEvents,buttonPointerEvents:getComputedStyle(button).pointerEvents,cancelled:[...cancelled]}};
window.ready=true;
</script>`);
const portServer=createServer();await new Promise(done=>portServer.listen(0,"127.0.0.1",done));const port=portServer.address().port;await new Promise(done=>portServer.close(done));
const relay=await startDirectLabRelay({port:0,publicDir:web});
let browser,page;const report={scope:"isolated synthetic UI only",cases:[],cleanup:[]};
try{
  browser=spawn(findChrome(),["--headless=new",...buildReceiverChromeArgs(resolve(dir,"chrome-profile"),port,"about:blank",{startMinimized:false})],{stdio:"ignore",windowsHide:true});
  report.browserPid=browser.pid;
  page=await new CdpPage(port).open();await page.navigate(new URL("/fixture.html",relay.url).href);
  for(let attempt=0;attempt<50&&!await page.evaluate("window.ready===true");attempt++)await new Promise(done=>setTimeout(done,100));
  assert.equal(await page.evaluate("window.ready===true"),true);
  await page.evaluate("document.fonts.ready");
  for(const scenario of [{name:"wide-light",width:1440,height:900,x:480,y:360,route:"drop"},{name:"narrow-edge",width:360,height:740,x:348,y:680,route:"drop"},{name:"blocked-dark",width:1440,height:900,x:1040,y:350,route:"blocked"}]){
    await page.call("Emulation.setDeviceMetricsOverride",{width:scenario.width,height:scenario.height,deviceScaleFactor:1,mobile:false});
    await page.call("Emulation.setEmulatedMedia",{features:[{name:"prefers-reduced-motion",value:"reduce"}]});
    await page.evaluate("new Promise(done=>requestAnimationFrame(()=>requestAnimationFrame(done)))");
    assert.equal(await page.evaluate(`preview(${scenario.x},${scenario.y},${JSON.stringify(scenario.route)})`),scenario.route==="blocked"?"none":"copy");
    await page.evaluate("new Promise(done=>requestAnimationFrame(()=>requestAnimationFrame(done)))");
    const state=await page.evaluate("snapshot()");
    assert.equal(state.fileReads,0);assert.equal(state.calls,0);assert.equal(state.pointerEvents,"none");
    assert.equal(state.display,"block");assert.equal(state.background,"rgba(0, 0, 0, 0)");assert.equal(state.border,"0px");
    assert.equal(state.marker.x+state.marker.width/2,scenario.x);assert.equal(state.marker.y+state.marker.height/2,scenario.y);
    const b=state.badge;assert.ok(b.x>=8&&b.y>=8&&b.x+b.width<=scenario.width-8&&b.y+b.height<=scenario.height-8);
    assert.ok(scenario.x<b.x||scenario.x>b.x+b.width||scenario.y<b.y||scenario.y>b.y+b.height);
    const screenshot=resolve(dir,scenario.name+".png");const result=await page.call("Page.captureScreenshot",{format:"png",captureBeyondViewport:false});writeFileSync(screenshot,Buffer.from(result.data,"base64"));report.cases.push({...scenario,state,screenshot});
  }
  for(const cleanup of ["blur","escape","drop","leave-empty-types","disable"]){
    await page.evaluate("preview(500,400)");
    await page.evaluate(cleanup==="disable"?"ui.setEnabled(false)":cleanup==="escape"?"window.dispatchEvent(new KeyboardEvent('keydown',{key:'Escape'}))":cleanup==="leave-empty-types"?"window.dispatchEvent(new DragEvent('dragleave',{clientX:0,clientY:0}))":cleanup==="drop"?"(()=>{const e=new DragEvent('drop',{cancelable:true});Object.defineProperty(e,'dataTransfer',{value:{types:['Files'],items:[]}});window.dispatchEvent(e)})()":"window.dispatchEvent(new Event('blur'))");
    const state=await page.evaluate("snapshot()");assert.equal(state.display,"none");assert.equal(state.badge.width,0);assert.equal(state.marker.width,0);report.cleanup.push({cleanup,state});await page.evaluate("ui.setEnabled(true)");
  }
  assert.equal(await page.evaluate("preview(500,400,'save')"),"copy");assert.equal((await page.evaluate("snapshot()")).mode,"save");
  for(const scenario of [{name:"large-wide-sending",width:1440,height:900,x:480,y:360,state:"sending"},{name:"large-narrow-attaching",width:360,height:740,x:348,y:680,state:"attaching"},{name:"large-wide-error",width:1440,height:900,x:1040,y:350,state:"failed"}]){
    await page.call("Emulation.setDeviceMetricsOverride",{width:scenario.width,height:scenario.height,deviceScaleFactor:1,mobile:false});
    await page.evaluate("new Promise(done=>requestAnimationFrame(()=>requestAnimationFrame(done)))");
    await page.evaluate(`sendLarge(${scenario.x},${scenario.y});transferState(${JSON.stringify(scenario.state)})`);
    await page.evaluate("new Promise(done=>requestAnimationFrame(()=>requestAnimationFrame(done)))");
    const state=await page.evaluate("progressSnapshot()");assert.equal(state.display,"grid");assert.equal(state.state,scenario.state);assert.equal(state.pointerEvents,"none");assert.equal(state.buttonPointerEvents,"auto");
    const b=state.badge;assert.ok(b.x>=8&&b.y>=8&&b.x+b.width<=scenario.width-8&&b.y+b.height<=scenario.height-8);assert.ok(scenario.x<b.x||scenario.x>b.x+b.width||scenario.y<b.y||scenario.y>b.y+b.height);
    assert.equal(state.title,scenario.state==='sending'?'Sending files to PC… 50%':scenario.state==='attaching'?'Attaching…':"Couldn't drop files on PC");
    const screenshot=resolve(dir,scenario.name+".png");const result=await page.call("Page.captureScreenshot",{format:"png",captureBeyondViewport:false});writeFileSync(screenshot,Buffer.from(result.data,"base64"));report.cases.push({...scenario,state,screenshot});
  }
  await page.evaluate("sendLarge(500,400);transferState('attaching');document.querySelector('#file-drop-progress button').focus()");
  await page.call("Input.dispatchKeyEvent",{type:"keyDown",key:"Enter",code:"Enter",text:"\r",windowsVirtualKeyCode:13});await page.call("Input.dispatchKeyEvent",{type:"keyUp",key:"Enter",code:"Enter",windowsVirtualKeyCode:13});
  let state=await page.evaluate("progressSnapshot()");assert.equal(state.title,"File drop cancelled");assert.deepEqual(state.cancelled,[1]);report.cleanup.push({cleanup:"keyboard-batch-cancel",state});
  await page.evaluate("document.querySelector('#file-drop-progress button').click()");assert.equal((await page.evaluate("progressSnapshot()")).display,"none");
  for(const finalState of ["done","reset"]){await page.evaluate(`sendLarge(500,400);transferState(${JSON.stringify(finalState)})`);state=await page.evaluate("progressSnapshot()");assert.equal(finalState==='done'?state.title:state.display,finalState==='done'?'Dropped on PC':'none');report.cleanup.push({cleanup:finalState,state})}
  await page.evaluate("ui.setEnabled(false)");assert.equal((await page.evaluate("progressSnapshot()")).display,"none");
  report.ok=true;
}catch(error){report.ok=false;report.error=error.stack;process.exitCode=1}
finally{
  try{await page?.call("Browser.close")}catch{}page?.close();
  if(browser?.pid)spawnSync("taskkill",["/PID",String(browser.pid),"/T","/F"],{stdio:"ignore",windowsHide:true});
  await relay.close();report.finishedAt=new Date().toISOString();writeFileSync(resolve(dir,"report.json"),JSON.stringify(report,null,2));console.log(JSON.stringify({ok:report.ok,dir,error:report.error},null,2));
}
