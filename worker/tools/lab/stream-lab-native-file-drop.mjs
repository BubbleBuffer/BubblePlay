// Local desktop acceptance: web FileTransferClient -> real Windows OLE -> Chrome.
// Synthetic files only; visible destination and isolated headless receiver.
import { spawn, spawnSync } from "node:child_process";
import { createHash, randomUUID } from "node:crypto";
import { closeSync, existsSync, mkdirSync, openSync, readFileSync, readdirSync, writeFileSync } from "node:fs";
import { createServer } from "node:net";
import { resolve } from "node:path";
import { pathToFileURL } from "node:url";
import { parseArgs } from "node:util";
import { deflateSync } from "node:zlib";
import { startDirectLabRelay } from "./stream-lab-direct-relay.mjs";
import { buildLabHostUrl, buildReceiverChromeArgs, buildReceiverUrl, CdpPage } from "./stream-lab-live.mjs";
import { findChrome } from "./stream-lab-runtime.mjs";

const root = resolve(import.meta.dirname, "../../..");
const delay = ms => new Promise(done => setTimeout(done, ms));
async function bounded(operation, label, ms = 5000) {
  let timer;
  try { return await Promise.race([operation,new Promise((_,reject)=>{timer=setTimeout(()=>reject(new Error(`timeout: ${label}`)),ms)})]); }
  finally { clearTimeout(timer); }
}
const hash = bytes => createHash("sha256").update(bytes).digest("hex");
function syntheticPng() {
  const chunk=(name,data)=>{
    const body=Buffer.concat([Buffer.from(name),data]);let crc=0xffff_ffff;
    for(const byte of body){crc^=byte;for(let bit=0;bit<8;bit++)crc=(crc>>>1)^((crc&1)?0xedb8_8320:0)}
    const length=Buffer.alloc(4),checksum=Buffer.alloc(4);length.writeUInt32BE(data.length);checksum.writeUInt32BE((crc^0xffff_ffff)>>>0);
    return Buffer.concat([length,body,checksum]);
  };
  const header=Buffer.alloc(13);header.writeUInt32BE(32,0);header.writeUInt32BE(32,4);header[8]=8;header[9]=6;
  const pixels=Buffer.alloc(32*(1+32*4));
  for(let y=0;y<32;y++)for(let x=0;x<32;x++){const at=y*129+1+x*4;pixels[at]=x*8;pixels[at+1]=y*8;pixels[at+2]=128;pixels[at+3]=255}
  return Buffer.concat([Buffer.from([137,80,78,71,13,10,26,10]),chunk("IHDR",header),chunk("IDAT",deflateSync(pixels)),chunk("IEND",Buffer.alloc(0))]);
}
const matchesFiles=(actual,expected)=>actual.length===expected.length&&expected.every((e,i)=>{
  const a=actual[i];return a?.name===e.name&&a.size===e.size&&a.hash===e.hash&&(e.type===undefined||a.type===e.type);
});
const fixture = `<!doctype html><title>BubblePlay native drop fixture</title>
<style>html,body{margin:0;height:100%;background:#edf8ef;color:#123;font:28px sans-serif}body{display:grid;place-content:center}</style>
<div>Drop synthetic files here</div><script>
window.received=[];window.fixtureError=null;window.fixtureEvents=[];window.fixtureReads=[];window.contentCompleteAt=null;
for(const type of ['dragenter','dragover','dragleave','drop'])document.addEventListener(type,e=>{
if(fixtureEvents.length>=128)fixtureEvents.shift();fixtureEvents.push({type,trusted:e.isTrusted,types:Array.from(e.dataTransfer?.types||[]),x:e.clientX,y:e.clientY,at:Date.now()})});
document.addEventListener('dragover',e=>{e.preventDefault();e.dataTransfer.dropEffect='copy'});
document.addEventListener('drop',async e=>{e.preventDefault();try{
for(const f of e.dataTransfer.files){const reading={name:f.name,startedAt:Date.now()};fixtureReads.push(reading);
const b=await f.arrayBuffer();reading.bytesCompleteAt=Date.now();const h=await crypto.subtle.digest('SHA-256',b);reading.hashCompleteAt=Date.now();
received.push({name:f.name,size:f.size,type:f.type,hash:Array.from(new Uint8Array(h),x=>x.toString(16).padStart(2,'0')).join('')})}
window.contentCompleteAt=Date.now();
document.querySelector('div').textContent=received.length+' files received';}catch(e){fixtureError=String(e)}});
</script>`;

async function freePort() {
  const server = createServer();
  await new Promise((accept, reject) => { server.once("error", reject); server.listen(0, "127.0.0.1", accept); });
  const port = server.address().port;
  await new Promise(done => server.close(done));
  return port;
}

// A PID may disappear or be reused after Browser.close. Revalidate the exact
// spawned executable plus this run's private profile/run id before terminating
// its process tree; never target a browser or host by image name alone.
function stopOwnedProcess(child, executable, runArgument) {
  if(!child?.pid)return {closed:true,method:'not-started'};
  const literal=value=>String(value).replaceAll("'","''");
  const script=`$ErrorActionPreference='Stop'
$owned=Get-CimInstance Win32_Process -Filter 'ProcessId=${child.pid}'
if(-not $owned){@{closed=$true;pid=${child.pid};method='already-exited'}|ConvertTo-Json -Compress;exit 0}
if($owned.ExecutablePath -ne '${literal(executable)}' -or -not $owned.CommandLine.Contains('${literal(runArgument)}')){throw 'owned process identity changed; refusing termination'}
& taskkill.exe /PID ${child.pid} /T /F | Out-Null
for($i=0;$i -lt 20;$i++){if(-not(Get-CimInstance Win32_Process -Filter 'ProcessId=${child.pid}')){@{closed=$true;pid=${child.pid};method='exact-owned-process-tree'}|ConvertTo-Json -Compress;exit 0};Start-Sleep -Milliseconds 100}
throw 'owned process did not exit'`;
  const result=spawnSync('powershell.exe',['-NoProfile','-NonInteractive','-Command',script],{encoding:'utf8',windowsHide:true,timeout:10000});
  if(result.status!==0)throw new Error(`owned process cleanup failed: ${(result.stderr||result.error?.message||'no result').trim().slice(-600)}`);
  return JSON.parse(result.stdout.trim());
}

/** Select only Explorer's exact owned scratch-folder window. No global input,
 * clipboard, user-folder navigation, or unrelated window enumeration output.
 */
export function explorerFixtureWindow(folder, region, close = false, expectedHwnd = null, previousWindows = [], positionAndVerify = true) {
  if(close)return closeExplorerFixture(folder,expectedHwnd);
  const literal = value => String(value).replaceAll("'", "''");
  const script = `
$ErrorActionPreference='Stop'
$scratchFolder='${literal(folder)}'
$shell=New-Object -ComObject Shell.Application
$matches=@($shell.Windows() | Where-Object {
  try { $_.Document.Folder.Self.Path -eq $scratchFolder } catch { $false }
})
if($matches.Count -eq 0){exit 3}
if($matches.Count -ne 1){throw 'owned Explorer fixture identity is ambiguous'}
$owned=$matches[0]
if(@(${previousWindows.join(",")}).Contains([long]$owned.HWND)){throw 'Explorer reused an existing window; fixture is not independently owned'}
${expectedHwnd !== null ? `if([long]$owned.HWND -ne ${expectedHwnd}){throw 'owned Explorer HWND identity changed'}` : ""}
if(${positionAndVerify ? "$false" : "$true"}){@{hwnd=[long]$owned.HWND;folder=$scratchFolder} | ConvertTo-Json -Compress;exit 0}
Add-Type -TypeDefinition @'
using System;
using System.Runtime.InteropServices;
public class ExplorerFixtureGeometry {
  [StructLayout(LayoutKind.Sequential)] public struct Rect { public int Left,Top,Right,Bottom; }
  [StructLayout(LayoutKind.Sequential)] public struct Point { public int X,Y; }
  [DllImport("user32.dll")] public static extern bool MoveWindow(IntPtr h,int x,int y,int w,int z,bool repaint);
  [DllImport("user32.dll")] public static extern bool SetForegroundWindow(IntPtr h);
  [DllImport("user32.dll")] public static extern bool ShowWindow(IntPtr h,int command);
  [DllImport("user32.dll")] public static extern bool IsWindowVisible(IntPtr h);
  [DllImport("user32.dll")] public static extern IntPtr GetForegroundWindow();
  [DllImport("user32.dll")] public static extern IntPtr WindowFromPoint(Point p);
  [DllImport("user32.dll")] public static extern IntPtr GetAncestor(IntPtr h,uint flags);
  [DllImport("user32.dll")] public static extern bool GetClientRect(IntPtr h,out Rect r);
  [DllImport("user32.dll")] public static extern bool ClientToScreen(IntPtr h,ref Point p);
  [DllImport("user32.dll")] public static extern IntPtr SetThreadDpiAwarenessContext(IntPtr c);
}
'@
[void][ExplorerFixtureGeometry]::SetThreadDpiAwarenessContext([IntPtr](-4))
$hwnd=[IntPtr]([long]$owned.HWND)
[void][ExplorerFixtureGeometry]::ShowWindow($hwnd,1)
if(-not [ExplorerFixtureGeometry]::MoveWindow($hwnd,${region.x + 60},${region.y + 60},${Math.max(300, Math.min(region.width - 120, 1000))},${Math.max(300, Math.min(region.height - 120, 750))},$true)){throw 'cannot position owned Explorer fixture'}
[void][ExplorerFixtureGeometry]::ShowWindow($hwnd,5)
[void][ExplorerFixtureGeometry]::SetForegroundWindow($hwnd)
$stableSamples=0;$lastGeometry=''
for($attempt=0;$attempt -lt 50;$attempt++){
  $rect=New-Object ExplorerFixtureGeometry+Rect
  if(-not [ExplorerFixtureGeometry]::GetClientRect($hwnd,[ref]$rect)){throw 'cannot read Explorer fixture geometry'}
  $point=New-Object ExplorerFixtureGeometry+Point
  $point.X=[int](($rect.Right-$rect.Left)*0.65)
  $point.Y=[int](($rect.Bottom-$rect.Top)*0.65)
  if(-not [ExplorerFixtureGeometry]::ClientToScreen($hwnd,[ref]$point)){throw 'cannot map Explorer fixture point'}
  $visible=[ExplorerFixtureGeometry]::IsWindowVisible($hwnd)
  $foregroundRoot=[ExplorerFixtureGeometry]::GetAncestor([ExplorerFixtureGeometry]::GetForegroundWindow(),2)
  $pointRoot=[ExplorerFixtureGeometry]::GetAncestor([ExplorerFixtureGeometry]::WindowFromPoint($point),2)
  $geometry="$($rect.Right),$($rect.Bottom),$($point.X),$($point.Y)"
  if($visible -and $rect.Right -gt 0 -and $rect.Bottom -gt 0 -and $foregroundRoot -eq $hwnd -and $pointRoot -eq $hwnd){
    if($geometry -eq $lastGeometry){$stableSamples++}else{$stableSamples=1}
  }else{$stableSamples=0}
  $lastGeometry=$geometry
  if($stableSamples -ge 2){break}
  Start-Sleep -Milliseconds 100
}
if($stableSamples -lt 2){throw "owned Explorer destination not stable/visible/owned; refusing drop: hwnd=$hwnd visible=$visible foreground=$foregroundRoot pointRoot=$pointRoot point=$($point.X),$($point.Y)"}
@{hwnd=[long]$hwnd;visible=$visible;stableSamples=$stableSamples;foregroundRoot=[long]$foregroundRoot;pointRoot=[long]$pointRoot;x=$point.X;y=$point.Y;folder=$scratchFolder;clientWidth=$rect.Right;clientHeight=$rect.Bottom;method='exact Shell folder, visible stable geometry, foreground root and physical point ownership verified'} | ConvertTo-Json -Compress
`;
  const result = spawnSync("powershell.exe",["-NoProfile","-NonInteractive","-Command",script],{encoding:"utf8",windowsHide:true,timeout:10000});
  if (result.status === 3) return null;
  if (result.status !== 0) throw new Error(`owned Explorer fixture unavailable: ${(result.stderr || result.error?.message || "no result").trim().slice(-600)}`);
  return JSON.parse(result.stdout.trim());
}

// The HWND comes only from fresh-window discovery against the prior Shell
// inventory. Revalidate its exact folder identity before each close request.
export function closeExplorerFixture(folder, expectedHwnd) {
  if(!Number.isSafeInteger(expectedHwnd)||expectedHwnd<=0)throw new Error("exact owned Explorer HWND is required for cleanup");
  const script=`
$ErrorActionPreference='Stop'
$scratchFolder='${String(folder).replaceAll("'","''")}'
$expectedHwnd=[IntPtr](${expectedHwnd})
$shell=New-Object -ComObject Shell.Application
Add-Type -TypeDefinition @'
using System;using System.Runtime.InteropServices;
public class ExplorerFixtureCleanup {
  [DllImport("user32.dll")] public static extern bool IsWindow(IntPtr h);
  [DllImport("user32.dll")] public static extern bool PostMessage(IntPtr h,uint message,IntPtr w,IntPtr l);
}
'@
function OwnedFolderWindows {
  @($shell.Windows() | Where-Object {try{$_.Document.Folder.Self.Path -eq $scratchFolder}catch{$false}})
}
function ExactOwnedWindow {
  $matches=@(OwnedFolderWindows)
  if($matches.Count -ne 1 -or [long]$matches[0].HWND -ne [long]$expectedHwnd){throw 'exact scratch folder and new HWND no longer match; refusing cleanup action'}
  return $matches[0]
}
function ClosureState {
  $windowExists=[ExplorerFixtureCleanup]::IsWindow($expectedHwnd)
  $count=@(OwnedFolderWindows).Count
  return @{closed=(-not $windowExists -and $count -eq 0);ownedHwndStillExists=$windowExists;exactFolderWindowCount=$count}
}
function WaitForClosure {
  for($attempt=0;$attempt -lt 15;$attempt++){
    $state=ClosureState
    if($state.closed){return $state}
    Start-Sleep -Milliseconds 100
  }
  return $state
}
$quitRequested=$false;$wmClosePosted=$false
$state=ClosureState
if(-not $state.closed){
  $owned=ExactOwnedWindow
  $owned.Quit();$quitRequested=$true
  $state=WaitForClosure
  if(-not $state.closed){
    [void](ExactOwnedWindow)
    $wmClosePosted=[ExplorerFixtureCleanup]::PostMessage($expectedHwnd,16,[IntPtr]::Zero,[IntPtr]::Zero)
    if(-not $wmClosePosted){throw 'WM_CLOSE failed for exact owned Explorer fixture'}
    $state=WaitForClosure
  }
}
if(-not $state.closed){throw "owned Explorer cleanup incomplete: hwnd=$expectedHwnd exists=$($state.ownedHwndStillExists) exactFolderWindowCount=$($state.exactFolderWindowCount)"}
$state.ownedHwnd=[long]$expectedHwnd;$state.quitRequested=$quitRequested;$state.wmClosePosted=$wmClosePosted
$state | ConvertTo-Json -Compress
`;
  const result=spawnSync("powershell.exe",["-NoProfile","-NonInteractive","-Command",script],{encoding:"utf8",windowsHide:true,timeout:10000});
  if(result.status!==0)throw new Error(`owned Explorer cleanup failed: ${(result.stderr||result.error?.message||"no result").trim().slice(-600)}`);
  return JSON.parse(result.stdout.trim());
}

function shellWindowHandles() {
  const result=spawnSync("powershell.exe",["-NoProfile","-NonInteractive","-Command",
    "$ErrorActionPreference='Stop';$shell=New-Object -ComObject Shell.Application;ConvertTo-Json -Compress -InputObject @($shell.Windows() | ForEach-Object {[long]$_.HWND})"],
    {encoding:"utf8",windowsHide:true,timeout:10000});
  if(result.status!==0)throw new Error("cannot establish existing Explorer window identities");
  return JSON.parse(result.stdout.trim());
}

// Shared by full native acceptance and Explorer-only fixture diagnostics.
// The caller records exact cleanup ownership before any presentation check.
export async function openExplorerFixture(folder, region, onDiscovered) {
  if(typeof onDiscovered !== "function")throw new Error("Explorer fixture cleanup ownership callback is required");
  const previousWindows=shellWindowHandles();
  spawn("explorer.exe",["/n,",folder],{stdio:"ignore",windowsHide:false});
  const deadline=Date.now()+20000;
  let observed;
  while(Date.now()<deadline){
    observed=explorerFixtureWindow(folder,region,false,null,previousWindows,false);
    if(observed)break;
    await delay(200);
  }
  if(!observed)throw new Error("timeout: owned scratch Explorer window");
  onDiscovered(observed.hwnd);
  await delay(600);
  return explorerFixtureWindow(folder,region,false,observed.hwnd,previousWindows);
}

function chromeFixturePoint(pid, title, region, { offsetX = 80, offsetY = 80, expectedHwnd = null, manualFocusMs = 0 } = {}) {
  const script = `
$ErrorActionPreference='Stop'
Add-Type -TypeDefinition @'
using System;
using System.Text;
using System.Collections.Generic;
using System.Runtime.InteropServices;
public class ChromeFixtureGeometry {
  public delegate bool EnumProc(IntPtr h,IntPtr p);
  [StructLayout(LayoutKind.Sequential)] public struct Rect { public int Left,Top,Right,Bottom; }
  [StructLayout(LayoutKind.Sequential)] public struct Point { public int X,Y; }
  [DllImport("user32.dll")] public static extern bool EnumWindows(EnumProc f,IntPtr p);
  [DllImport("user32.dll")] public static extern uint GetWindowThreadProcessId(IntPtr h,out uint id);
  [DllImport("user32.dll",CharSet=CharSet.Unicode)] public static extern int GetWindowText(IntPtr h,StringBuilder text,int n);
  [DllImport("user32.dll")] public static extern bool IsWindowVisible(IntPtr h);
  [DllImport("user32.dll")] public static extern bool MoveWindow(IntPtr h,int x,int y,int w,int z,bool repaint);
  [DllImport("user32.dll")] public static extern bool SetForegroundWindow(IntPtr h);
  [DllImport("user32.dll")] public static extern IntPtr GetForegroundWindow();
  [DllImport("user32.dll")] public static extern IntPtr GetAncestor(IntPtr h,uint flags);
  [DllImport("user32.dll")] public static extern IntPtr WindowFromPoint(Point point);
  [DllImport("user32.dll")] public static extern bool ShowWindow(IntPtr h,int command);
  [DllImport("user32.dll")] public static extern bool AttachThreadInput(uint from,uint to,bool attach);
  [DllImport("kernel32.dll")] public static extern uint GetCurrentThreadId();
  [StructLayout(LayoutKind.Sequential)] public struct Message { public IntPtr Hwnd; public uint Id; public UIntPtr WParam; public IntPtr LParam; public uint Time; public Point Point; public uint Private; }
  [DllImport("user32.dll")] public static extern bool PeekMessage(out Message message,IntPtr hwnd,uint min,uint max,uint remove);
  [DllImport("user32.dll")] public static extern bool GetClientRect(IntPtr h,out Rect r);
  [DllImport("user32.dll")] public static extern bool ClientToScreen(IntPtr h,ref Point p);
  [DllImport("user32.dll")] public static extern IntPtr SetThreadDpiAwarenessContext(IntPtr c);
  public static long Find(uint pid,string title) {
    var windows=new List<IntPtr>();
    EnumWindows((h,p)=>{uint owner;GetWindowThreadProcessId(h,out owner);
      if(owner==pid && IsWindowVisible(h)) {var text=new StringBuilder(2048);GetWindowText(h,text,text.Capacity);
        if(text.ToString().Contains(title)) windows.Add(h);}return true;},IntPtr.Zero);
    if(windows.Count!=1) throw new Exception("expected exactly one owned Chrome fixture window; found "+windows.Count);
    return windows[0].ToInt64();
  }
  public static bool ActivateOwned(IntPtr hwnd,uint expectedPid) {
    uint actualPid;uint targetThread=GetWindowThreadProcessId(hwnd,out actualPid);
    if(actualPid!=expectedPid || targetThread==0) throw new Exception("owned Chrome identity changed before activation");
    ShowWindow(hwnd,9);
    if(SetForegroundWindow(hwnd) && GetAncestor(GetForegroundWindow(),2)==hwnd) return true;
    uint current=GetCurrentThreadId(),foregroundPid;
    uint foregroundThread=GetWindowThreadProcessId(GetForegroundWindow(),out foregroundPid);
    // Create this temporary helper's message queue before attaching. No input
    // is synthesized: attachment only enables activating the owned window.
    Message message;PeekMessage(out message,IntPtr.Zero,0,0,0);
    bool foregroundAttached=false,targetAttached=false;
    try {
      if(foregroundThread!=0 && foregroundThread!=current) foregroundAttached=AttachThreadInput(current,foregroundThread,true);
      if(targetThread!=current && targetThread!=foregroundThread) targetAttached=AttachThreadInput(current,targetThread,true);
      SetForegroundWindow(hwnd);
      return GetAncestor(GetForegroundWindow(),2)==hwnd;
    } finally {
      if(targetAttached) AttachThreadInput(current,targetThread,false);
      if(foregroundAttached) AttachThreadInput(current,foregroundThread,false);
    }
  }
}
'@
[void][ChromeFixtureGeometry]::SetThreadDpiAwarenessContext([IntPtr](-4))
$hwnd=[IntPtr]([ChromeFixtureGeometry]::Find(${pid},'${title.replaceAll("'", "''")}'))
${expectedHwnd === null ? "" : `if([long]$hwnd -ne ${expectedHwnd}){throw 'owned Chrome HWND identity changed; refusing window move'}`}
if(-not [ChromeFixtureGeometry]::MoveWindow($hwnd,${region.x + offsetX},${region.y + offsetY},850,650,$true)){throw 'cannot position owned Chrome fixture'}
$stableSamples=0;$lastGeometry='';$activated=$false
for($attempt=0;$attempt -lt ${manualFocusMs ? Math.ceil(manualFocusMs / 100) : 40};$attempt++){
  if([ChromeFixtureGeometry]::Find(${pid},'${title.replaceAll("'", "''")}') -ne [long]$hwnd){throw 'owned Chrome fixture identity changed'}
  $activated=${manualFocusMs ? '[ChromeFixtureGeometry]::GetAncestor([ChromeFixtureGeometry]::GetForegroundWindow(),2) -eq $hwnd' : `[ChromeFixtureGeometry]::ActivateOwned($hwnd,${pid})`}
  $rect=New-Object ChromeFixtureGeometry+Rect
  if(-not [ChromeFixtureGeometry]::GetClientRect($hwnd,[ref]$rect)){throw 'cannot read Chrome fixture geometry'}
  $point=New-Object ChromeFixtureGeometry+Point
  $point.X=[int](($rect.Right-$rect.Left)/2);$point.Y=[int](($rect.Bottom-$rect.Top)/2)
  if(-not [ChromeFixtureGeometry]::ClientToScreen($hwnd,[ref]$point)){throw 'cannot map Chrome fixture point'}
  $visible=[ChromeFixtureGeometry]::IsWindowVisible($hwnd)
  $foreground=[ChromeFixtureGeometry]::GetForegroundWindow()
  $foregroundRoot=[ChromeFixtureGeometry]::GetAncestor($foreground,2)
  $pointLeaf=[ChromeFixtureGeometry]::WindowFromPoint($point)
  $pointRoot=[ChromeFixtureGeometry]::GetAncestor($pointLeaf,2)
  $geometry="$($rect.Right),$($rect.Bottom),$($point.X),$($point.Y)"
  if($activated -and $visible -and $foregroundRoot -eq $hwnd -and $pointRoot -eq $hwnd -and $rect.Right -gt 0 -and $rect.Bottom -gt 0){
    if($geometry -eq $lastGeometry){$stableSamples++}else{$stableSamples=1}
  }else{$stableSamples=0}
  $lastGeometry=$geometry
  if($stableSamples -ge 2){break}
  Start-Sleep -Milliseconds 100
}
if($stableSamples -lt 2){throw "owned Chrome fixture not activated/visible/point-owned; refusing preview: hwnd=$hwnd activated=$activated foreground=$foreground foregroundRoot=$foregroundRoot pointLeaf=$pointLeaf pointRoot=$pointRoot"}
@{hwnd=[long]$hwnd;pid=${pid};x=$point.X;y=$point.Y;clientWidth=$rect.Right;clientHeight=$rect.Bottom;visible=$visible;activated=$activated;stableSamples=$stableSamples;foreground=[long]$foreground;foregroundRoot=[long]$foregroundRoot;pointLeaf=[long]$pointLeaf;pointRoot=[long]$pointRoot;method='exact owned PID/title/HWND, bounded activation, stable visible physical point and foreground root verified'} | ConvertTo-Json -Compress
`;
  const result=spawnSync("powershell.exe",["-NoProfile","-NonInteractive","-Command",script],{encoding:"utf8",windowsHide:true,timeout:manualFocusMs+10000});
  if(result.status!==0)throw new Error(`Chrome fixture geometry unavailable: ${(result.stderr||result.error?.message||"no result").trim().slice(-600)}`);
  return JSON.parse(result.stdout.trim());
}

export function bounds(selector) {
  const script = `Add-Type -TypeDefinition 'using System;using System.Runtime.InteropServices;public class LabDpi {[DllImport("user32.dll")] public static extern IntPtr SetThreadDpiAwarenessContext(IntPtr c);}'
[void][LabDpi]::SetThreadDpiAwarenessContext([IntPtr](-4))
Add-Type -AssemblyName System.Windows.Forms
$s=[System.Windows.Forms.Screen]::AllScreens | Where-Object {$_.DeviceName -eq '${String(selector).replaceAll("'", "''")}'} | Select-Object -First 1
if(-not $s){throw 'selected captured monitor unavailable'}
"$($s.Bounds.X),$($s.Bounds.Y),$($s.Bounds.Width),$($s.Bounds.Height)"`;
  const result = spawnSync("powershell.exe", ["-NoProfile", "-NonInteractive", "-Command", script], { encoding:"utf8", windowsHide:true });
  const [x,y,width,height] = result.stdout.trim().split(",").map(Number);
  if (![x,y,width,height].every(Number.isFinite)) throw new Error("monitor bounds unavailable");
  return {x,y,width,height};
}

// Local input-hook probes operate only over this run's exact foreground Chrome
// fixture. They neither activate another window nor move to another point.
function injectOwnedChromeInput(pid,hwnd,title,kind) {
  if(!['stationary-move','wheel'].includes(kind))throw new Error('unsupported owned input probe');
  const literal=value=>String(value).replaceAll("'","''");
  const script=`$ErrorActionPreference='Stop'
Add-Type -TypeDefinition @'
using System;using System.Text;using System.Runtime.InteropServices;
public class OwnedChromeInputProbe {
 [StructLayout(LayoutKind.Sequential)] public struct Point {public int X,Y;}
 [StructLayout(LayoutKind.Sequential)] public struct Mouse {public int X,Y;public uint Data,Flags,Time;public UIntPtr Extra;}
 [StructLayout(LayoutKind.Explicit)] public struct Union {[FieldOffset(0)]public Mouse Mouse;}
 [StructLayout(LayoutKind.Sequential)] public struct Input {public uint Type;public Union Data;}
 [StructLayout(LayoutKind.Sequential)] public struct LastInput {public uint Size,Time;}
 [DllImport("user32.dll")] public static extern uint GetWindowThreadProcessId(IntPtr h,out uint pid);
 [DllImport("user32.dll",CharSet=CharSet.Unicode)] public static extern int GetWindowText(IntPtr h,StringBuilder s,int count);
 [DllImport("user32.dll")] public static extern IntPtr GetForegroundWindow();
 [DllImport("user32.dll")] public static extern IntPtr GetAncestor(IntPtr h,uint flags);
 [DllImport("user32.dll")] public static extern IntPtr WindowFromPoint(Point point);
 [DllImport("user32.dll")] public static extern bool GetCursorPos(out Point point);
 [DllImport("user32.dll")] public static extern int GetSystemMetrics(int index);
 [DllImport("user32.dll")] public static extern bool GetLastInputInfo(ref LastInput info);
 [DllImport("user32.dll")] public static extern IntPtr SetThreadDpiAwarenessContext(IntPtr context);
 [DllImport("user32.dll",SetLastError=true)] public static extern uint SendInput(uint count,Input[] input,int size);
 public static long[] Inject(uint pid,long handle,string title,bool stationary) {
  SetThreadDpiAwarenessContext(new IntPtr(-4));IntPtr hwnd=new IntPtr(handle);uint actualPid;
  GetWindowThreadProcessId(hwnd,out actualPid);var text=new StringBuilder(2048);GetWindowText(hwnd,text,text.Capacity);
  Point point;if(!GetCursorPos(out point))throw new Exception("cannot read owned fixture cursor");
  IntPtr foreground=GetForegroundWindow(),foregroundRoot=GetAncestor(foreground,2),leaf=WindowFromPoint(point),root=GetAncestor(leaf,2);
  if(actualPid!=pid||!text.ToString().Contains(title)||foregroundRoot!=hwnd||root!=hwnd)
   throw new Exception("owned input probe identity/foreground/point mismatch; no input injected");
  var last=new LastInput{Size=(uint)Marshal.SizeOf(typeof(LastInput))};GetLastInputInfo(ref last);uint beforeTime=last.Time;
  var mouse=new Mouse();if(stationary)mouse.Flags=0x0001;else{mouse.Flags=0x0800;mouse.Data=120;}
  var inputs=new[]{new Input{Type=0,Data=new Union{Mouse=mouse}}};
  if(SendInput(1,inputs,Marshal.SizeOf(typeof(Input)))!=1)throw new Exception("owned input probe SendInput failed");
  System.Threading.Thread.Sleep(100);GetLastInputInfo(ref last);int count=1,mode=0;
  if(stationary&&last.Time==beforeTime){
   Point current;uint stillPid;GetWindowThreadProcessId(hwnd,out stillPid);
   if(!GetCursorPos(out current)||current.X!=point.X||current.Y!=point.Y||stillPid!=pid||GetAncestor(GetForegroundWindow(),2)!=hwnd||GetAncestor(WindowFromPoint(current),2)!=hwnd)
    throw new Exception("owned stationary fallback identity changed; no fallback input injected");
   int left=GetSystemMetrics(76),top=GetSystemMetrics(77),width=GetSystemMetrics(78),height=GetSystemMetrics(79);
   if(width<=0||height<=0)throw new Exception("virtual desktop geometry unavailable");
   mouse.X=(int)(((long)(point.X-left)*65536+32768)/width);mouse.Y=(int)(((long)(point.Y-top)*65536+32768)/height);
   mouse.Flags=0x0001|0x8000|0x4000;inputs[0]=new Input{Type=0,Data=new Union{Mouse=mouse}};
   if(SendInput(1,inputs,Marshal.SizeOf(typeof(Input)))!=1)throw new Exception("owned stationary fallback SendInput failed");
   System.Threading.Thread.Sleep(100);GetLastInputInfo(ref last);count=2;mode=1;
  }
  if(stationary&&last.Time==beforeTime)throw new Exception("owned stationary injection did not advance last-input timestamp; race proof unavailable");
  Point after;if(!GetCursorPos(out after))throw new Exception("cannot verify owned fixture cursor after input");
  if(stationary&&(point.X!=after.X||point.Y!=after.Y))throw new Exception("stationary probe changed cursor position; regression proof invalid");
  return new long[]{foreground.ToInt64(),foregroundRoot.ToInt64(),leaf.ToInt64(),root.ToInt64(),point.X,point.Y,after.X,after.Y,beforeTime,last.Time,count,mode};
 }
}
'@
$proof=[OwnedChromeInputProbe]::Inject(${pid},${hwnd},'${literal(title)}',${kind==='stationary-move'?'$true':'$false'})
@{kind='${kind}';at=[DateTimeOffset]::UtcNow.ToUnixTimeMilliseconds();count=$proof[10];absoluteFallback=($proof[11]-eq 1);hwnd=${hwnd};pid=${pid};foreground=$proof[0];foregroundRoot=$proof[1];pointLeaf=$proof[2];pointRoot=$proof[3];x=$proof[4];y=$proof[5];afterX=$proof[6];afterY=$proof[7];pointUnchanged=($proof[4]-eq $proof[6]-and $proof[5]-eq $proof[7]);lastInputBefore=$proof[8];lastInputAfter=$proof[9]}|ConvertTo-Json -Compress`;
  const result=spawnSync('powershell.exe',['-NoProfile','-NonInteractive','-Command',script],{encoding:'utf8',windowsHide:true,timeout:10000});
  if(result.status!==0)throw new Error(`owned local input probe failed: ${(result.stderr||result.error?.message||'no result').trim().slice(-700)}`);
  return JSON.parse(result.stdout.trim());
}

async function main() {
  const { values } = parseArgs({ options: {
    "candidate-host": { type: "string", default: resolve(root,"host/target/release/bubbleplay-host.exe") },
    explorer: { type: "boolean", default: false },
    "result-timeout-ms": { type: "string", default: "20000" },
    "manual-focus-ms": { type: "string", default: "0" },
  } });
  const resultTimeout=Number(values["result-timeout-ms"]);
  const manualFocusMs=Number(values["manual-focus-ms"]);
  if(!Number.isInteger(manualFocusMs)||manualFocusMs<0||manualFocusMs>60000)throw new Error("manual focus wait must be 0..60000 ms");
  if(!Number.isInteger(resultTimeout)||resultTimeout<1000||resultTimeout>60000)throw new Error("result timeout must be 1000..60000 ms");
  const candidateHost = resolve(values["candidate-host"]);
  if (!existsSync(candidateHost)) throw new Error(`candidate host missing: ${candidateHost}`);
  const runId = randomUUID();
  const dir = resolve(root,"artifacts/stream-lab-native-file-drop",runId);
  mkdirSync(dir,{recursive:true});
  const fixturePath = resolve(dir,"target.html");
  const fixtureTitle=`BubblePlay native drop fixture ${runId}`;
  writeFileSync(fixturePath,fixture.replace("BubblePlay native drop fixture",fixtureTitle));
  const report = {runId,startedAt:new Date().toISOString(),scope:"synthetic web sender to actual Windows OLE destinations",
    candidateHost, candidateHostSha256:hash(readFileSync(candidateHost)),steps:{}};
  const save = () => writeFileSync(resolve(dir,"report.json"),JSON.stringify(report,null,2));
  save();
  const relay = await startDirectLabRelay({port:0,publicDir:resolve(root,"worker/public")});
  let host,receiverProcess,targetProcess,receiver,target,explorerFolder,explorerOwnedHwnd,chrome;
  const until = async (check,label,ms=30000) => {
    const end=Date.now()+ms;
    while(Date.now()<end){if(await bounded(Promise.resolve().then(check),label,Math.min(5000,end-Date.now())).catch(()=>false))return;await delay(100)}
    throw new Error(`timeout: ${label}`);
  };
  try {
    chrome = findChrome();
    if(!chrome)throw new Error("Chrome unavailable");
    const out=openSync(resolve(dir,"host.stdout.log"),"a"),err=openSync(resolve(dir,"host.stderr.log"),"a");
    host=spawn(candidateHost,["--mode","serve","--worker",buildLabHostUrl(relay.url,runId).href],{
      cwd:resolve(root,"host"),stdio:["ignore",out,err],windowsHide:true,
      env:{...process.env,RUST_LOG:"info",BUBBLEPLAY_TOKEN:"local-lab-relay-placeholder",BUBBLEPLAY_HOST_GATE_TOKEN:"local-lab-relay-placeholder",
        BUBBLEPLAY_DOWNLOADS_DIR:resolve(dir,"downloads"),BUBBLEPLAY_SENDTO_DIR:resolve(dir,"sendto"),BUBBLEPLAY_FILE_CLIPBOARD:"0"}});
    closeSync(out);closeSync(err);
    report.hostPid=host.pid;save();
    host.once("exit",code=>{report.hostExit=code;save()});
    const receiverPort=await freePort(),targetPort=await freePort();
    report.receiverPort=receiverPort;report.targetPort=targetPort;save();
    receiverProcess=spawn(chrome,["--headless=new",...buildReceiverChromeArgs(resolve(dir,"receiver-profile"),receiverPort,"about:blank",{startMinimized:false})],{stdio:"ignore",windowsHide:true});
    receiver=await new CdpPage(receiverPort).open();
    await receiver.call("Emulation.setDeviceMetricsOverride",{width:1920,height:1080,deviceScaleFactor:1,mobile:false});
    await receiver.navigate(buildReceiverUrl(relay.url,"wss",{labRun:runId,adaptive:true}).href);
    await until(()=>receiver.evaluate("window.__bubbleplayDiagnostics?.snapshot?.()?.framesDrawn > 0"),"first frame");
    await until(()=>receiver.evaluate("window.__bubbleplayFiles?.dropCapability?.() === true"),"native drop capability");
    await until(()=>receiver.evaluate("window.__bubbleplayFiles?.dropPreviewCapability?.() === true"),"native hover preview capability");
    const monitors=await receiver.evaluate("window.__bubbleplayDiagnostics.snapshot().signaling.monitors");
    const monitor=monitors.find(m=>m.selected)||monitors[0];
    if(!monitor)throw new Error("no selected monitor reported by host");
    const region=bounds(monitor.selector);report.monitor={...monitor,region};
    targetProcess=spawn(chrome,[`--user-data-dir=${resolve(dir,"target-profile")}`,"--no-first-run","--no-default-browser-check",`--remote-debugging-port=${targetPort}`,
      `--window-position=${region.x+80},${region.y+80}`,"--window-size=850,650",`--app=${pathToFileURL(fixturePath).href}`],{stdio:"ignore"});
    target=await new CdpPage(targetPort).open();
    await until(()=>target.evaluate("Array.isArray(window.received)"),"drop fixture");
    if(await target.evaluate("location.href")!==pathToFileURL(fixturePath).href)throw new Error("isolated Chrome target identity mismatch");
    await target.call("Page.bringToFront");
    await delay(600);
    // Choose the center of this specific fixture window, then normalize to the
    // selected host monitor. Receiver's captured generation is passed unchanged.
    const center=chromeFixturePoint(targetProcess.pid,fixtureTitle,region,{manualFocusMs});
    await delay(300);
    const normalizedPoint=center=>{
      const point={x:Math.round((center.x-region.x)*65535/(region.width-1)),y:Math.round((center.y-region.y)*65535/(region.height-1))};
      if(!Number.isFinite(point.x)||!Number.isFinite(point.y)||point.x<0||point.x>65535||point.y<0||point.y>65535)throw new Error("drop destination outside captured monitor");
      return point;
    };
    const point=normalizedPoint(center);
    report.chromeTarget={center,point};save();
    const bytes=Buffer.from("BubblePlay native drop synthetic fixture\n");
    const png=syntheticPng();
    const expected=[{name:"native-small.txt",size:bytes.length,hash:hash(bytes)},{name:"native-empty.txt",size:0,hash:hash(Buffer.alloc(0))},
      {name:"screenshot.png",size:png.length,type:"image/png",hash:hash(png)}];
    const pointCode=point=>`const media=document.querySelector('#visual video')||document.querySelector('#screen');const rect=media.getBoundingClientRect();
      const width=media.videoWidth||media.width,height=media.videoHeight||media.height,scale=Math.min(rect.width/width,rect.height/height);
      const w=width*scale,h=height*scale,left=rect.left+(rect.width-w)/2,top=rect.top+(rect.height-h)/2;
      const clientX=left+w*${point.x}/65535,clientY=top+h*${point.y}/65535;`;
    await receiver.evaluate(`(()=>{window.__labPreviewSource={fileReads:0,controls:[],uploads:[]};
      const originalSend=WebSocket.prototype.send;
      WebSocket.prototype.send=function(data){if(typeof data==='string'){try{const message=JSON.parse(data);
        if(message.type==='file-drag-preview'){if(__labPreviewSource.controls.length>=128)__labPreviewSource.controls.shift();__labPreviewSource.controls.push({at:Date.now(),message})}}catch{}}
        else if(window.__labLargeGate){const bytes=data instanceof ArrayBuffer?new Uint8Array(data):ArrayBuffer.isView(data)?new Uint8Array(data.buffer,data.byteOffset,data.byteLength):null;
          if(window.__labInputBeat?.localOnly&&bytes?.length&&bytes[0]!==0x42){
            const meaningful=bytes[0]===0||bytes[0]===11||bytes[0]===12||bytes[0]===13&&!(bytes[1]&1)||bytes[0]===4&&bytes[1]!==0||bytes[0]===9&&bytes[7]!==0;
            if(meaningful)__labInputBeat.meaningfulRemotePackets.push({at:Date.now(),type:bytes[0],size:bytes.length});
          }
          if(window.__labInputBeat&&bytes?.length===7&&bytes[0]===8){const view=new DataView(bytes.buffer,bytes.byteOffset,bytes.byteLength);
            __labInputBeat.packets.push({at:Date.now(),type:8,seq:view.getUint16(1,true),x:view.getUint16(3,true),y:view.getUint16(5,true)})}
          if(window.__labInputBeat&&bytes?.length===11&&bytes[0]===7){const view=new DataView(bytes.buffer,bytes.byteOffset,bytes.byteLength);
            __labInputBeat.packets.push({at:Date.now(),type:7,seq:view.getUint16(1,true),wheelX:view.getInt16(7,true),wheelY:view.getInt16(9,true)})}
          if(bytes?.length>=16&&bytes[0]===0x42&&bytes[1]===0x50&&bytes[2]===0x46&&bytes[3]===0x31){
            const id=new DataView(bytes.buffer,bytes.byteOffset,bytes.byteLength).getUint32(8,true);
            if(window.__bubbleplayFiles.client.rows.some(r=>r.transferId===id&&r.name===__labLargeGate.name)){
              if(bytes[4]===3){__labLargeGate.senderBytes=(__labLargeGate.senderBytes||0)+bytes.length-16;__labLargeGate.lastDataAt=Date.now()}
              if(bytes[4]===5)__labLargeGate.senderDoneAt=Date.now();
            }
          }
        }
        return originalSend.call(this,data)};
      const client=window.__bubbleplayFiles.client,originalUpload=client.upload;
      client.upload=function(files,...args){__labPreviewSource.uploads.push({at:Date.now(),names:files.map(f=>f.name),lastPreviewAction:__labPreviewSource.controls.at(-1)?.message.action});return originalUpload.call(this,files,...args)};
      return true})()`);
    const dispatchPreview=async(point,type="dragover")=>receiver.evaluate(`(()=>{${pointCode(point)}
      const data={types:['Files'],items:['text/plain','text/plain','image/png'].map(type=>({kind:'file',type,
        getAsFile(){__labPreviewSource.fileReads++;throw Error('protected preview file read')},
        get name(){__labPreviewSource.fileReads++;throw Error('protected preview name read')}}))};
      Object.defineProperty(data,'files',{get(){__labPreviewSource.fileReads++;throw Error('protected preview Files read')}});
      const event=new DragEvent(${JSON.stringify(type)},{bubbles:true,cancelable:true,clientX,clientY});Object.defineProperty(event,'dataTransfer',{value:data});
      document.querySelector('#input-surface').dispatchEvent(event);return true})()`);
    const sourcePreview=()=>receiver.evaluate("({...__labPreviewSource,rows:window.__bubbleplayFiles.client.transfers})");
    const targetPreview=()=>target.evaluate("({received,fixtureEvents,fixtureError})");
    const noPreviewFiles=(source,destination)=>source.fileReads===0&&source.uploads.length===0&&source.rows.length===0&&
      destination.received.length===0&&!destination.fixtureEvents.some(e=>e.type==='drop');
    const movedPoint=normalizedPoint({x:center.x+60,y:center.y+40});
    report.chromeTarget.previewMovedPoint=movedPoint;
    report.steps.previewHover={status:"starting",point,movedPoint};save();
    await dispatchPreview(point,"dragenter");await dispatchPreview(point);
    await until(()=>target.evaluate("fixtureEvents.some(e=>e.type==='dragenter'&&e.trusted)&&fixtureEvents.some(e=>e.type==='dragover'&&e.trusted)"),"trusted native preview entry",8000);
    const entry=await targetPreview(),initialSource=await sourcePreview();
    if(!noPreviewFiles(initialSource,entry))throw new Error("hover read or uploaded files before local drop");
    const heldAt=Date.now();await delay(3300);
    const stationary=await targetPreview(),stationarySource=await sourcePreview();
    const heldStationaryMs=Date.now()-heldAt;
    if(!noPreviewFiles(stationarySource,stationary)||stationary.fixtureEvents.at(-1)?.type==='dragleave'||
      stationarySource.controls.filter(e=>e.message.action==='update').length<6)throw new Error("stationary native preview did not survive keepalive without files");
    const initialOver=entry.fixtureEvents.find(e=>e.type==='dragover'&&e.trusted);
    await dispatchPreview(movedPoint);
    await until(()=>target.evaluate(`fixtureEvents.some(e=>e.type==='dragover'&&e.trusted&&(Math.abs(e.x-${initialOver.x})>20||Math.abs(e.y-${initialOver.y})>20))`),"native preview moved inside target",5000);
    const moved=await targetPreview();
    await receiver.evaluate("document.querySelector('#input-surface').dispatchEvent(new DragEvent('dragleave',{bubbles:true,clientX:0,clientY:0}))");
    await until(()=>target.evaluate("fixtureEvents.some(e=>e.type==='dragleave'&&e.trusted)"),"trusted native preview cancellation",5000);
    const cancelled=await targetPreview(),cancelledSource=await sourcePreview();
    report.steps.previewHover={status:"cancelled",point,movedPoint,heldStationaryMs,entry,initialSource,stationary,stationarySource,moved,cancelled,cancelledSource,
      nativePreviewLog:["host.stdout.log","host.stderr.log"].flatMap(name=>readFileSync(resolve(dir,name),"utf8").split(/\r?\n/).filter(line=>/preview/i.test(line))).slice(-24),
      ok:noPreviewFiles(cancelledSource,cancelled)&&cancelledSource.controls.at(-1)?.message.action==='cancel'};
    save();if(!report.steps.previewHover.ok)throw new Error("cancelled native preview left files or failed to send cancel");
    await target.evaluate("received=[];fixtureEvents=[];fixtureError=null;true");
    await dispatchPreview(movedPoint,"dragenter");await dispatchPreview(movedPoint);
    await until(()=>target.evaluate("fixtureEvents.some(e=>e.type==='dragenter'&&e.trusted)"),"second trusted native preview",8000);
    const replacementEntry=await targetPreview(),replacementSource=await sourcePreview();
    if(!noPreviewFiles(replacementSource,replacementEntry)||replacementSource.controls.at(-1)?.message.dragId===initialSource.controls[0]?.message.dragId)throw new Error("second hover materialized files or reused cancelled drag identity");
    report.steps.previewReplacement={status:"hovering-before-real-drop",entry:replacementEntry,sourceBeforeDrop:replacementSource};save();
    const sendDrop=async(point,files)=>{
      const ids=await receiver.evaluate(`(()=>{const client=window.__bubbleplayFiles.client;const previous=new Set(client.transfers.map(r=>r.id));const data=new DataTransfer();
        for(const f of ${JSON.stringify(files)})data.items.add(new File([f.base64?Uint8Array.from(atob(f.base64),c=>c.charCodeAt(0)):f.text],f.name,{type:f.type||''}));
        ${pointCode(point)}
        document.querySelector('#input-surface').dispatchEvent(new DragEvent('drop',{bubbles:true,cancelable:true,dataTransfer:data,
          clientX,clientY}));
        return client.transfers.filter(r=>!previous.has(r.id)).map(r=>r.id)})()`);
      if(ids.length!==files.length)throw new Error(`drop created ${ids.length} transfer rows for ${files.length} files`);
      await until(()=>receiver.evaluate(`window.__bubbleplayFiles.client.transfers.filter(r=>${JSON.stringify(ids)}.includes(r.id)).length === ${files.length} && window.__bubbleplayFiles.client.transfers.filter(r=>${JSON.stringify(ids)}.includes(r.id)).every(r=>['done','failed','cancelled'].includes(r.state))`),"native drop results",resultTimeout);
      return receiver.evaluate(`window.__bubbleplayFiles.client.transfers.filter(r=>${JSON.stringify(ids)}.includes(r.id))`);
    };
    const rows=await sendDrop(movedPoint,[{name:"native-small.txt",text:bytes.toString()},{name:"native-empty.txt",text:""},
      {name:"screenshot.png",base64:png.toString("base64"),type:"image/png"}]);
    report.steps.chrome={expected,rows};save();
    if(rows.every(r=>r.state==='done'))await until(()=>target.evaluate(`received.length === ${expected.length} || fixtureError !== null`),"actual target bytes",10000);
    const received=await target.evaluate("received");
    const fixtureEvents=await target.evaluate("fixtureEvents");
    report.steps.chrome={expected,received,rows,fixtureEvents,fixtureError:await target.evaluate("fixtureError"),ok:matchesFiles(received,expected)&&
      fixtureEvents.some(e=>e.type==='drop'&&e.trusted)&&rows.length===expected.length&&rows.every(r=>r.state==='done'&&r.purpose==='drop')};
    const replacementResult=await sourcePreview();
    report.steps.previewReplacement={...report.steps.previewReplacement,status:"real-files-delivered",sourceAfterDrop:replacementResult,
      ok:report.steps.chrome.ok&&replacementResult.fileReads===0&&replacementResult.uploads.length===1&&replacementResult.uploads[0].lastPreviewAction==='cancel'&&
        !received.some(f=>f.name.startsWith('incoming-file-'))};
    save();
    // Exercise a real synchronous virtual-file reader waiting on healthy data
    // beyond the 3-second entry limit. This source overrides slice/arrayBuffer,
    // the API actually used by FileTransferClient, without changing other files.
    if(report.steps.chrome.ok){
      await target.evaluate("received=[];fixtureEvents=[];fixtureError=null;true");
      const text="BubblePlay healthy native drop delayed bytes\n";
      const expected=[{name:"native-delayed.txt",size:Buffer.byteLength(text),hash:hash(Buffer.from(text))}];
      const ids=await receiver.evaluate(`(()=>{const client=window.__bubbleplayFiles.client;
        const captured=client.rows.findLast(r=>r.purpose==='drop'&&r.state==='done')?.meta?.drop;
        if(!captured)throw new Error('no previously captured native drop point');
        class DelayedFile extends File {slice(start,end){const blob=super.slice(start,end);return {arrayBuffer:async()=>{
          if(start===0)await new Promise(done=>setTimeout(done,5000));return blob.arrayBuffer()}}}}
        return client.upload([new DelayedFile([${JSON.stringify(text)}],'native-delayed.txt')],'drop',{...captured})})()`);
      await until(()=>receiver.evaluate(`window.__bubbleplayFiles.client.transfers.filter(r=>${JSON.stringify(ids)}.includes(r.id)).length === 1 && window.__bubbleplayFiles.client.transfers.filter(r=>${JSON.stringify(ids)}.includes(r.id)).every(r=>['done','failed','cancelled'].includes(r.state))`),"healthy delayed native drop",resultTimeout);
      const rows=await receiver.evaluate(`window.__bubbleplayFiles.client.transfers.filter(r=>${JSON.stringify(ids)}.includes(r.id))`);
      report.steps.delayedChrome={delayMs:5000,expected,rows};save();
      if(rows.every(r=>r.state==='done'))await until(()=>target.evaluate("received.length === 1 || fixtureError !== null"),"delayed actual target bytes",10000);
      const received=await target.evaluate("received"),fixtureEvents=await target.evaluate("fixtureEvents");
      report.steps.delayedChrome={...report.steps.delayedChrome,received,fixtureEvents,ok:matchesFiles(received,expected)&&
        fixtureEvents.some(e=>e.type==='drop'&&e.trusted)&&rows.length===1&&rows[0].state==='done'&&rows[0].finishedAt-rows[0].startedAt>=5000};
      save();
    }
    // Real OLE must not start while a large batch's synthetic upload is blocked.
    // Enter through the UI drop handler, preserving preview cancellation and its
    // progress/Cancel badge; only the sender's first chunk read is gated.
    const largeSize=12*1024*1024,largeByte=0x5a;
    const largeHash=hash(Buffer.alloc(largeSize,largeByte));
    const resetTarget=()=>target.evaluate("received=[];fixtureEvents=[];fixtureReads=[];contentCompleteAt=null;fixtureError=null;true");
    const largeRows=ids=>receiver.evaluate(`window.__bubbleplayFiles.client.transfers.filter(r=>${JSON.stringify(ids)}.includes(r.id))`);
    const finishLarge=async(ids,label)=>{
      await until(async()=>{const rows=await largeRows(ids);return rows.length===ids.length&&rows.every(r=>['done','failed','cancelled'].includes(r.state))},label,resultTimeout);
      return largeRows(ids);
    };
    const largeTarget=()=>target.evaluate("({received,fixtureEvents,fixtureReads,contentCompleteAt,fixtureError})");
    const nativeLogs=()=>["host.stdout.log","host.stderr.log"].flatMap(name=>readFileSync(resolve(dir,name),"utf8").split(/\r?\n/).filter(line=>/native.*drop|drop.*native/i.test(line)));
    const assertNoActualDrop=snapshot=>{
      const leave=snapshot.fixtureEvents.findIndex(e=>e.type==='dragleave'&&e.trusted);
      return leave>=0&&snapshot.received.length===0&&snapshot.fixtureReads.length===0&&snapshot.fixtureError===null&&
        !snapshot.fixtureEvents.some(e=>e.type==='drop')&&
        !snapshot.fixtureEvents.slice(leave+1).some(e=>['dragenter','dragover'].includes(e.type));
    };
    const startLarge=async(name,{holdMs=3300,heartbeats=false}={})=>{
      await resetTarget();
      await dispatchPreview(point,'dragenter');await dispatchPreview(point);
      await until(()=>target.evaluate("fixtureEvents.some(e=>e.type==='dragenter'&&e.trusted)"),'large trusted preview',8000);
      const logBefore=nativeLogs().length;
      const ids=await receiver.evaluate(`(()=>{const client=window.__bubbleplayFiles.client,previous=new Set(client.transfers.map(r=>r.id));
        let release;const wait=new Promise(done=>release=done);
        window.__labLargeGate={name:${JSON.stringify(name)},createdAt:Date.now(),enteredAt:null,releasedAt:null,senderBytes:0,senderDoneAt:null,
          release(){if(this.releasedAt===null){this.releasedAt=Date.now();release()}}};
        const bytes=new Uint8Array(${largeSize});bytes.fill(${largeByte});
        class GatedFile extends File {slice(start,end){const blob=super.slice(start,end);return {arrayBuffer:async()=>{
          if(start===0){__labLargeGate.enteredAt=Date.now();await wait}return blob.arrayBuffer()}}}}
        const original=client.upload;
        client.upload=function(files,...args){return original.call(this,files.map(f=>f.name===${JSON.stringify(name)}?
          new GatedFile([bytes],f.name,{type:'application/octet-stream'}):f),...args)};
        try{const data=new DataTransfer();data.items.add(new File([bytes],${JSON.stringify(name)},{type:'application/octet-stream'}));
          ${pointCode(point)}
          document.querySelector('#input-surface').dispatchEvent(new DragEvent('drop',{bubbles:true,cancelable:true,dataTransfer:data,clientX,clientY}));
        }finally{client.upload=original}
        return client.transfers.filter(r=>!previous.has(r.id)).map(r=>r.id)})()`);
      if(ids.length!==1)throw new Error('large UI drop did not create exactly one row');
      await until(()=>receiver.evaluate("__labLargeGate.enteredAt !== null"),'gated large first chunk read');
      await until(()=>target.evaluate("fixtureEvents.some(e=>e.type==='dragleave'&&e.trusted)"),'large preview cancelled before staging',8000);
      if(heartbeats)await receiver.evaluate(`(()=>{const row=window.__bubbleplayFiles.client.rows.find(r=>r.id===${ids[0]});
        const drop=row?.meta?.drop;if(!drop)throw Error('large heartbeat lacks current captured drop coordinates');
        const media=document.querySelector('#visual video')||document.querySelector('#screen'),rect=media.getBoundingClientRect();
        const width=media.videoWidth||media.width,height=media.videoHeight||media.height,scale=Math.min(rect.width/width,rect.height/height);
        const w=width*scale,h=height*scale,left=rect.left+(rect.width-w)/2,top=rect.top+(rect.height-h)/2;
        // MouseEvent stores integer client coordinates. Round the inverse map
        // first: implicit truncation can shift a captured drop by one pixel.
        const clientX=Math.round(left+w*drop.x/65535),clientY=Math.round(top+h*drop.y/65535);
        const predicted={x:Math.round((clientX-left)/w*65535),y:Math.round((clientY-top)/h*65535)};
        if(predicted.x!==drop.x||predicted.y!==drop.y)throw Error('captured drop is not exactly representable by current receiver mouse coordinates');
        window.__labInputBeat={startedAt:Date.now(),drop:{...drop},clientPoint:{x:clientX,y:clientY},contentRect:{left,top,width:w,height:h},predicted,packets:[],timer:null};
        const send=()=>document.querySelector('#input-surface').dispatchEvent(new MouseEvent('mousemove',{
          bubbles:true,clientX,clientY}));
        send();__labInputBeat.timer=setInterval(send,166);return true})()`);
      const heldAt=Date.now();let nativeStationaryInput=null;
      if(heartbeats){
        await delay(6000);
        const settled=await receiver.evaluate("__labInputBeat.packets.length");
        if(settled<20)throw new Error('owned stationary native probe lacks settled real ABS input');
        // Delayed application of unchanged motion must remain motion even after
        // the host's remote-input loop flag has cleared. No cursor reposition.
        nativeStationaryInput=injectOwnedChromeInput(targetProcess.pid,center.hwnd,fixtureTitle,'stationary-move');
        await delay(Math.max(0,holdMs-(Date.now()-heldAt)));
      }else await delay(holdMs);
      const inputHeartbeat=heartbeats?await receiver.evaluate("clearInterval(__labInputBeat.timer);({...__labInputBeat,timer:undefined,finishedAt:Date.now()})"):null;
      const held=await largeTarget(),rows=await largeRows(ids),gate=await receiver.evaluate("({...__labLargeGate,release:undefined})");
      const badge=await receiver.evaluate("(()=>{const b=document.querySelector('#file-drop-progress');return b?{hidden:b.hidden,state:b.dataset.state,text:b.textContent,cancel:b.querySelector('button')?.textContent}:null})()");
      const evidence={name,size:largeSize,expectedHash:largeHash,heldMs:Date.now()-heldAt,inputHeartbeat,nativeStationaryInput,gate,held,rows,badge,
        noActualOleWhileBlocked:assertNoActualDrop(held),nativeLogBefore:logBefore};
      report.steps.largePreparing=evidence;save();
      if(!evidence.noActualOleWhileBlocked||rows.some(r=>['done','failed','cancelled'].includes(r.state)))throw new Error('large drop entered actual OLE or terminated while upload was blocked');
      if(heartbeats&&(inputHeartbeat.packets.length<50||inputHeartbeat.packets.some(p=>p.type!==8||p.x!==inputHeartbeat.drop.x||p.y!==inputHeartbeat.drop.y)||
        inputHeartbeat.packets.some((p,i)=>i>0&&(((p.seq-inputHeartbeat.packets[i-1].seq)&0xffff)===0||((p.seq-inputHeartbeat.packets[i-1].seq)&0xffff)>=32768))))throw new Error('long preparation lacks stationary sequence-ordered real ABS-v2 heartbeat proof');
      if(!badge||badge.hidden||badge.cancel!=='Cancel')throw new Error('large preparing drop lacks visible Cancel badge');
      return {ids,evidence,logBefore};
    };
    const releaseLarge=()=>receiver.evaluate("__labLargeGate.release();true");
    const staged=await startLarge('native-large-staged.bin',{holdMs:12000,heartbeats:true});
    await releaseLarge();
    const stagedRows=await finishLarge(staged.ids,'fully staged large native drop');
    if(stagedRows.every(r=>r.state==='done'))await until(()=>target.evaluate("received.length===1||fixtureError!==null"),'large Chrome content complete',10000);
    const stagedDestination=await largeTarget(),stagedGate=await receiver.evaluate("({...__labLargeGate,release:undefined})");
    const trustedDrop=stagedDestination.fixtureEvents.find(e=>e.type==='drop'&&e.trusted);
    const stagedNativeLog=nativeLogs().slice(staged.logBefore);
    const nativeVerified=stagedNativeLog.some(line=>line.includes('native file drop accepted with verified contents'));
    report.steps.largeChrome={...staged.evidence,rows:stagedRows,destination:stagedDestination,gate:stagedGate,
      nativeLog:stagedNativeLog,nativeVerified,timings:{localDropAt:stagedGate.createdAt,uploadUnblockedAt:stagedGate.releasedAt,senderDoneAt:stagedGate.senderDoneAt,
        actualDropEventAt:trustedDrop?.at??null,contentCompleteAt:stagedDestination.contentCompleteAt,
        actualDropToContentMs:trustedDrop&&stagedDestination.contentCompleteAt!==null?stagedDestination.contentCompleteAt-trustedDrop.at:null},
      ok:staged.evidence.noActualOleWhileBlocked&&matchesFiles(stagedDestination.received,[{name:staged.evidence.name,size:largeSize,type:'application/octet-stream',hash:largeHash}])&&
        !!trustedDrop&&stagedGate.senderBytes===largeSize&&stagedGate.senderDoneAt!==null&&trustedDrop.at>=stagedGate.senderDoneAt&&
        nativeVerified&&stagedRows.length===1&&stagedRows[0].state==='done'};
    save();if(!report.steps.largeChrome.ok)throw new Error('fully staged large drop did not reach exact owned Chrome with correct bytes');
    const interruptedLarge=await startLarge('native-large-interrupted.bin');
    await receiver.evaluate(`(()=>{window.__labInputBeat={packets:[]};${pointCode(point)}
      document.querySelector('#input-surface').dispatchEvent(new WheelEvent('wheel',{
        bubbles:true,cancelable:true,clientX,clientY,deltaY:120,deltaMode:0}));return true})()`);
    const interruptedRows=await finishLarge(interruptedLarge.ids,'meaningful remote wheel interrupts preparation');
    const wheelInput=await receiver.evaluate("({...__labInputBeat})");
    await releaseLarge();await delay(600);
    const interruptedDestination=await largeTarget();
    report.steps.largeInterrupted={...interruptedLarge.evidence,rows:interruptedRows,wheelInput,destination:interruptedDestination,
      nativeLog:nativeLogs().slice(interruptedLarge.logBefore),
      destinationFiles:existsSync(resolve(dir,'downloads'))?readdirSync(resolve(dir,'downloads')):[],
      ok:wheelInput.packets.some(p=>p.type===7&&p.wheelY!==0)&&interruptedRows.length===1&&interruptedRows[0].state==='failed'&&
        interruptedRows[0].error==='drop-interrupted'&&assertNoActualDrop(interruptedDestination)&&
        !(existsSync(resolve(dir,'downloads',interruptedLarge.evidence.name)))};
    save();if(!report.steps.largeInterrupted.ok)throw new Error('meaningful remote input did not cancel preparation without an actual drop');
    await receiver.evaluate("window.__labInputBeat={localOnly:true,packets:[],meaningfulRemotePackets:[]};true");
    const localWheelLarge=await startLarge('native-large-local-wheel.bin');
    const monitorReady=nativeLogs().slice(localWheelLarge.logBefore).some(line=>line.includes('prepared native drop input monitor ready'));
    if(!monitorReady)throw new Error('local wheel probe lacks ready native input-hook evidence');
    const localWheelInput=injectOwnedChromeInput(targetProcess.pid,center.hwnd,fixtureTitle,'wheel');
    const localWheelRows=await finishLarge(localWheelLarge.ids,'local wheel hook interrupts preparation');
    const localRemoteInput=await receiver.evaluate("({...__labInputBeat})");
    await releaseLarge();await delay(600);
    const localWheelDestination=await largeTarget();
    report.steps.largeLocalWheel={...localWheelLarge.evidence,monitorReady,localWheelInput,rows:localWheelRows,remoteInput:localRemoteInput,
      destination:localWheelDestination,nativeLog:nativeLogs().slice(localWheelLarge.logBefore),
      destinationFiles:existsSync(resolve(dir,'downloads'))?readdirSync(resolve(dir,'downloads')):[],
      ok:localRemoteInput.meaningfulRemotePackets.length===0&&!localRemoteInput.packets.some(p=>p.type===7&&(p.wheelX!==0||p.wheelY!==0))&&
        localWheelRows.length===1&&localWheelRows[0].state==='failed'&&localWheelRows[0].error==='drop-interrupted'&&
        assertNoActualDrop(localWheelDestination)&&!existsSync(resolve(dir,'downloads',localWheelLarge.evidence.name))};
    save();if(!report.steps.largeLocalWheel.ok)throw new Error('local wheel hook did not cancel preparation independently of remote meaningful input');
    await receiver.evaluate("window.__labInputBeat=null;true");
    const cancelledLarge=await startLarge('native-large-cancelled.bin');
    await receiver.evaluate("(()=>{const button=document.querySelector('#file-drop-progress button');if(!button||button.textContent!=='Cancel')throw Error('large Cancel control missing');button.click();return true})()");
    const cancelRows=await finishLarge(cancelledLarge.ids,'cancelled large drop');
    await releaseLarge();await delay(1200);
    const cancelDestination=await largeTarget();
    report.steps.largeCancelled={...cancelledLarge.evidence,rows:cancelRows,destination:cancelDestination,nativeLog:nativeLogs().slice(cancelledLarge.logBefore),
      destinationFiles:existsSync(resolve(dir,'downloads'))?readdirSync(resolve(dir,'downloads')):[],
      ok:cancelRows.length===1&&cancelRows[0].state==='cancelled'&&assertNoActualDrop(cancelDestination)&&
        !(existsSync(resolve(dir,'downloads',cancelledLarge.evidence.name)))};
    save();if(!report.steps.largeCancelled.ok)throw new Error('cancelled large preparation delivered files or entered OLE');
    const movedLarge=await startLarge('native-large-moved.bin');
    const movedWindow=chromeFixturePoint(targetProcess.pid,fixtureTitle,region,{offsetX:180,offsetY:130,expectedHwnd:center.hwnd});
    await delay(400);await releaseLarge();
    const movedRows=await finishLarge(movedLarge.ids,'moved large destination rejection');await delay(500);
    const movedDestination=await largeTarget();
    report.steps.largeMoved={...movedLarge.evidence,movedWindow,rows:movedRows,destination:movedDestination,nativeLog:nativeLogs().slice(movedLarge.logBefore),
      destinationFiles:existsSync(resolve(dir,'downloads'))?readdirSync(resolve(dir,'downloads')):[],
      ok:movedRows.length===1&&movedRows[0].state==='failed'&&movedRows[0].error==='drop-target-changed'&&assertNoActualDrop(movedDestination)&&
        !(existsSync(resolve(dir,'downloads',movedLarge.evidence.name)))};
    save();
    chromeFixturePoint(targetProcess.pid,fixtureTitle,region,{expectedHwnd:center.hwnd});await delay(300);
    if(!report.steps.largeMoved.ok)throw new Error('moved owned destination accepted a late large drop');
    // A stale generation must fail without a second target drop.
    const previousReceived=await target.evaluate("received.length");
    const staleIds=await receiver.evaluate(`window.__bubbleplayFiles.client.upload([new File(['stale'],'stale.txt')],'drop',{x:${point.x},y:${point.y},generation:4294967295,viewportRevision:0})`);
    await until(()=>receiver.evaluate(`window.__bubbleplayFiles.client.transfers.filter(r=>${JSON.stringify(staleIds)}.includes(r.id)).every(r=>['done','failed','cancelled'].includes(r.state))`),"stale rejection",10000);
    const staleRows=await receiver.evaluate(`window.__bubbleplayFiles.client.transfers.filter(r=>${JSON.stringify(staleIds)}.includes(r.id))`);
    report.steps.stale={rows:staleRows,ok:staleRows.length===1&&staleRows[0].state==='failed'&&staleRows[0].error==='drop-stale-view'&&(await target.evaluate("received.length"))===previousReceived};
    save();
    if(values.explorer&&report.steps.chrome.ok&&report.steps.delayedChrome?.ok){
      // Close only the already-accepted owned Chrome target so it cannot cover
      // the Explorer destination or consume its native drop.
      await bounded(target.call("Browser.close"),"accepted Chrome fixture shutdown");
      target.close();target=null;
      report.chromeFixtureClosedBeforeExplorer=true;save();
      const folder=resolve(dir,`explorer-${runId.slice(0,8)}`);
      mkdirSync(folder,{recursive:true});
      report.steps.explorer={status:"opening-owned-scratch-window",folder};save();
      // Explorer reuses its shell process. Never kill that process or unrelated
      // windows: cleanup closes only this selected scratch window.
      const observed=await openExplorerFixture(folder,region,hwnd=>{explorerFolder=folder;explorerOwnedHwnd=hwnd});
      report.steps.explorer={...report.steps.explorer,status:"destination-verified",observed};save();
      const explorerPoint=normalizedPoint(observed);
      const text="BubblePlay synthetic Explorer native drop\n";
      const expected=[{name:"explorer-small.txt",size:Buffer.byteLength(text),hash:hash(Buffer.from(text))},{name:"explorer-empty.txt",size:0,hash:hash(Buffer.alloc(0))}];
      const rows=await sendDrop(explorerPoint,[{name:expected[0].name,text},{name:expected[1].name,text:""}]);
      report.steps.explorer={...report.steps.explorer,status:"results",observed,point:explorerPoint,expected,rows};save();
      if(rows.every(r=>r.state==='done'))await until(()=>Promise.resolve(expected.every(f=>existsSync(resolve(folder,f.name)))),"Explorer copied files",15000);
      const actual=readdirSync(folder).map(name=>{const b=readFileSync(resolve(folder,name));return{name,size:b.length,hash:hash(b)}});
      report.steps.explorer.actual=actual;
      report.steps.explorer.ok=expected.every(e=>actual.some(a=>a.name===e.name&&a.size===e.size&&a.hash===e.hash))&&
        actual.length===expected.length&&rows.length===expected.length&&rows.every(r=>r.state==='done'&&r.purpose==='drop');
      save();
    }else if(values.explorer){
      report.steps.explorer={status:"skipped",reason:"native Chrome acceptance has not passed"};save();
    }
    report.ok=report.steps.previewHover.ok&&report.steps.previewReplacement.ok&&report.steps.chrome.ok&&report.steps.delayedChrome?.ok&&
      report.steps.largeChrome.ok&&report.steps.largeInterrupted.ok&&report.steps.largeLocalWheel.ok&&report.steps.largeCancelled.ok&&report.steps.largeMoved.ok&&report.steps.stale.ok&&(!values.explorer||report.steps.explorer.ok);
    if(!report.ok)process.exitCode=1;
  }catch(error){
    report.error={message:error.message};
    try{report.receiverFailureState=await bounded(receiver?.evaluate("({rows:window.__bubbleplayFiles?.client?.transfers,preview:window.__labPreviewSource,diagnostics:window.__bubbleplayDiagnostics?.snapshot?.()})"),"receiver failure diagnostics")}catch{}
    try{report.targetFailureState=await bounded(target?.evaluate("({received,fixtureError,fixtureEvents})"),"target failure diagnostics")}catch{}
    process.exitCode=1;
  }
  finally{
    try{await bounded(receiver?.evaluate("if(window.__labInputBeat?.timer)clearInterval(__labInputBeat.timer);true"),'stop owned input heartbeat',2000)}catch{}
    try{
      await bounded(receiver?.evaluate("window.dispatchEvent(new Event('blur'));true"),"end owned hover during cleanup",2000);
      report.previewEndRequestedDuringCleanup=true;
      await delay(300);
    }catch{}
    if(explorerFolder){
      report.explorerFixtureWindowClosed=false;
      try{
        report.explorerCleanup=closeExplorerFixture(explorerFolder,explorerOwnedHwnd);
        report.explorerFixtureWindowClosed=report.explorerCleanup.closed;
      }catch(error){report.explorerCleanupError=error.message;report.ok=false;process.exitCode=1}
    }
    for(const page of [target,receiver]){try{await bounded(page?.call("Browser.close"),"owned fixture browser shutdown")}catch{}page?.close()}
    report.ownedProcessCleanup=[];
    for(const [name,child,executable,argument] of [
      ['target',targetProcess,chrome,resolve(dir,'target-profile')],
      ['receiver',receiverProcess,chrome,resolve(dir,'receiver-profile')],
      ['host',host,candidateHost,runId],
    ]){
      try{report.ownedProcessCleanup.push({name,...stopOwnedProcess(child,executable,argument)})}
      catch(error){report.ownedProcessCleanup.push({name,closed:false,error:error.message});report.ok=false;process.exitCode=1}
    }
    await relay.close();report.finishedAt=new Date().toISOString();save();
    console.log(JSON.stringify({ok:report.ok,error:report.error,report:resolve(dir,"report.json")},null,2));
  }
}
if (process.argv[1] && resolve(process.argv[1]) === resolve(import.meta.filename)) await main();
