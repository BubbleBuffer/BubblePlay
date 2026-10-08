// Development only: freeze packets produced by the public browser modules.
import { mkdir, writeFile } from 'node:fs/promises';
import { encodeFrameV4, ackMessageV4, keyframeMessageV4 } from '../../worker/public/media-v4.js';
import { encodeOffer, encodeData, encodeDone, encodeRequest, encodeCancel } from '../../worker/public/file-transfer.js';
import { CLIPBOARD_IMAGE } from '../../worker/public/input.js';
const epoch='0123456789abcdef0123456789abcdef';
const hex=bytes=>Buffer.from(bytes).toString('hex');
const point={x:32767,y:20000,generation:1,viewportRevision:0};
const meta={name:'synthetic.txt',size:3,mime:'text/plain',mtimeMs:0,purpose:'drop',drop:point,batchId:1,batchCount:1,batchIndex:0};
const fixture={epoch,mediaHex:hex(encodeFrameV4({key:true,codec:1,generation:1,seq:0,captureTsUs:123n,data:new Uint8Array([1,2,3])})),ack:JSON.parse(ackMessageV4({generation:1,seq:0,recvTsMs:12.3,decodedSeq:0})),keyframe:JSON.parse(keyframeMessageV4('decoder:lost')),files:[encodeOffer(1,meta),encodeData(1,0,new Uint8Array([1,2,3])),encodeDone(1,new Uint8Array(32)),encodeRequest(2,1,0),encodeCancel(1,'cancelled')].map(hex),signals:[
{role:'client',message:{type:'client-log',seq:1,atMs:1,level:'info',event:'test',detail:{}}},
{role:'client',message:{type:'qos',sessionId:'synthetic',seq:1,atMs:1,rx:{},presentation:{}}},
{role:'client',message:{type:'media-control',action:'start',protocol:4,epoch,codec:'h264'}},
{role:'client',message:{type:'media-control',action:'mode',protocol:4,epoch,streamMode:'gaming'}},
{role:'client',message:{type:'media-control',action:'quality',protocol:4,epoch}},
{role:'client',message:{type:'first-frame',protocol:1,requestId:'fixture_1',phase:'request',transport:'wss',epoch}},
{role:'host',message:{type:'media-view',protocol:4,generation:1,desktopWidth:1920,desktopHeight:1080,sourceX:0,sourceY:0,sourceWidth:1920,sourceHeight:1080}},
{role:'host',message:{type:'monitor-list',protocol:4,monitors:[],current:'primary'}},
{role:'host',message:{type:'input-capabilities',protocol:1,scancode:true,clipboard:true,clipboardMaxBytes:1048576}},
{role:'host',message:{type:'clipboard-image',protocol:1,id:1,index:0,count:CLIPBOARD_IMAGE.maxChunks,mimeType:'image/png',text:'AQID'}},
{role:'client',message:{type:'file-drag-preview',protocol:1,epoch,dragId:1,action:'update',drop:point,types:['text/plain']}},
]};
await mkdir(new URL('../testdata/',import.meta.url),{recursive:true});
await writeFile(new URL('../testdata/worker-contract.json',import.meta.url),JSON.stringify(fixture,null,2)+'\n');
