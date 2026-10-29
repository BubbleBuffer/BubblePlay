# File transfer

File bytes use a dedicated WebSocket so an upload or download does not share the signaling/input or media queue. The host caps download send rate against media capacity. Transfer UI is hidden until the host advertises `fileTransfer: 1`.

## User behavior and boundaries

- Desktop browser drops on the stream use copy-only Windows OLE virtual files when the user-level host advertises `fileDrop: 1`. Compatible targets receive a data object; Explorer saves into its chosen folder and a browser upload target receives files. The target controls what happens after the copy.
- File-picker, mobile and older-host uploads save into `%USERPROFILE%\Downloads\BubblePlay`; paste uploads also place the batch on the clipboard and inject Ctrl+V while input is ready. Names are sanitized, collisions renamed, and saved files get Mark-of-the-Web. Files are never opened or executed automatically.
- Downloads are offers from files copied in Explorer or selected through Send To > BubblePlay. Only those offered files can be requested; the client cannot browse arbitrary host folders. Downloads wait for client acceptance.
- The browser uses File System Access when available; its Blob fallback supports files up to 256 MiB. Android writes verified downloads through MediaStore; see the [Android guide](../../android/README.md).

Browser downloads bind the offer ID at the download click, before the save picker opens. If the PC replaces or withdraws that offer while the picker is open, the browser downloads nothing and shows "The PC replaced these files. Try again."

A `fileDropPreview: 1` host can show provisional OLE hover before local release, using generated names and bounded MIME hints. Browsers protect actual filenames and bytes until the drop event. Preview supplies no contents or cache files; it is cancelled and fully cleaned up before a new data object receives real files.

Native drops bind the current session, media generation, viewport and drop position. Batches of 10 MiB or more are fully staged and verified before OLE commits; smaller batches can negotiate immediately while content reads wait for verification. Large-drop preparation retains the original native target identity and bounds, monitors input categories, and cancels on target/session/view changes or interfering presses/wheel/text. Pointer movement and releases are allowed. Window identity cannot establish that a website's DOM stayed unchanged. ACK progress means staging; native success means verified bytes and an accepted copy disposition, not completion of a later cloud upload.

The temporary native cache is private and carries Mark-of-the-Web. Destination apps control their copied files and may not preserve that alternate data stream. Cancelling stops further source reads; it does not remove files already copied by the destination.

## Implementation and checks

[`host/src/file_service.rs`](../../host/src/file_service.rs) owns host file sessions, [`file_transfer.rs`](../../host/src/file_transfer.rs) framing, and [`native_file_drop.rs`](../../host/src/native_file_drop.rs) the Windows target integration. Browser behavior lives in [`file-client.js`](../../worker/public/file-client.js), [`file-transfer.js`](../../worker/public/file-transfer.js) and [`file-ui.js`](../../worker/public/file-ui.js). The relay validates and pairs file sockets by epoch.

The setup/update scripts create Send To > BubblePlay. The tray stages requests under `%LOCALAPPDATA%\BubblePlay\sendto`; the host rejects old requests and skips folders. `BUBBLEPLAY_DOWNLOADS_DIR` and `BUBBLEPLAY_SENDTO_DIR` override those local locations. `BUBBLEPLAY_FILE_CLIPBOARD=0` disables clipboard file reading/placement and paste for isolated tests.

Rust, JavaScript and Kotlin exercise the shared fixture. The isolated Windows lab checks for this (`stream-lab-file-transfer`, `stream-lab-native-file-drop`) were removed at CUT-1 with the JavaScript Worker relay. Source checks and prior Chrome/Explorer evidence do not prove a fresh installation or native interoperability through the Rust relay; see [release status](../release-status.md).

## Wire contract (v1)

Capability: host capabilities gain `fileTransfer: 1`. Clients hide all file UI when it is absent.
Native drops additionally require `fileDrop: 1`. Older clients and mobile clients continue using save/paste.
Pre-release host hover additionally requires `fileDropPreview: 1` alongside both existing capabilities, and remains desktop-only.

Signaling (`/ws`, JSON; after the handshake each message is a `ctl` kind, with its type as `kind`):

- host → client `{"type":"file-offer","protocol":1,"offerId":u32,"items":[{"id":u32,"name":string,"size":u64}],"source":"clipboard"|"sendto"}`; a new offer replaces the previous one; `items: []` withdraws it.
- client → host `{"type":"file-drag-preview","protocol":1,"epoch":currentMediaEpoch,"dragId":positiveU32,"action":"update","drop":{"x":u16,"y":u16,"generation":nonzeroU32,"viewportRevision":u32},"types":[mimeHints]}`. One to 64 hints, each at most 96 ASCII graphic bytes; empty means unknown. No names, paths or bytes. Controls are at most 8192 UTF-8 bytes, current-epoch client-only, and never buffered for a later host. `action:"cancel"` omits `drop` and `types`. Client throttles motion to 200 ms and renews stationary hover every 500 ms. Host lease expires after two seconds without controls or 120 seconds overall; readiness/geometry/session changes cancel it. A refused/expired drag ID is not repeatedly restarted. Actual local drop cancels before upload; the final handover waits at most two seconds for complete native cleanup without blocking capture.

File socket: `/file-ws?pc=<id>&epoch=<mediaEpoch>` (viewer: ticket hello as on media sockets; a browser viewer also needs the gate cookie) and `/host-file-ws?pc=<id>&epoch=<mediaEpoch>` (host: its ticket from the `/host-ws` hello-ok, sent in the hello; no authorization header and no host gate token). The relay pairs one file-viewer with one file-host per epoch.

Text messages on the file socket (JSON):

- first message from either side `{"type":"hello","role":"file-viewer"|"file-host"}` (plus `ticket`, which the relay requires from both sides; `file-client` is refused); relay answers `{"type":"hello-ok"}` then `{"type":"file-peer","present":bool}`, and sends `file-peer` again when the other side arrives. Send OFFER/REQUEST only while the peer is present.
- `{"type":"ping"}` every 15 s while open; the relay answers `{"type":"pong"}` and never relays it.
- When one side closes, the relay closes the other with 1001 `file-peer-left`; a binary packet that fails validation closes the sender with 1008. REQUEST is accepted only from the file-viewer.
- The client picks every transferId (its OFFER for an upload, its REQUEST for a download). The host keeps its socket open while a viewer epoch is live and reconnects 1 s after a close; clients open on demand and close after 60 s without a transfer.

Binary messages: 16-byte header + payload.

| Offset | Field | |
|---|---|---|
| 0 | magic | ASCII `BPF1` |
| 4 | type u8 | see below |
| 5 | flags u8 | must be 0 in v1 |
| 6 | reserved u16 | must be 0 |
| 8 | transferId u32 LE | chosen by the sender of OFFER; unique per socket |
| 12 | seq u32 LE | DATA: chunk index from 0; otherwise 0 |

| Type | Name | Direction | Payload |
|---|---|---|---|
| 1 | OFFER | sender → receiver | UTF-8 JSON `{name,size,mime,mtimeMs,purpose:"save"|"paste"|"drop",batchId,batchIndex,batchCount,drop?}` |
| 2 | ACCEPT | receiver → sender | empty |
| 3 | DATA | sender → receiver | raw bytes, ≤ 65536 |
| 4 | ACK | receiver → sender | u64 LE total bytes received |
| 5 | DONE | sender → receiver | 32-byte SHA-256 of the file |
| 6 | CANCEL | either | UTF-8 JSON `{reason}` |
| 7 | RESULT | receiver → sender | UTF-8 JSON `{ok,error?}` |
| 8 | REQUEST | client → host | UTF-8 JSON `{offerId,id}`; host answers with OFFER (same transferId) |

Rules: sender keeps ≤ 1 MiB un-ACKed; receiver ACKs at least every 256 KiB or 100 ms; receiver verifies size and SHA-256 before RESULT ok and deletes partial files on failure or CANCEL; one transfer at a time per direction, others queue; limits 2 GiB per file, 64 files per batch, names ≤ 1024 UTF-8 bytes, control payloads ≤ 4096 bytes; the socket closes after 60 s idle and is reopened on demand; a socket drop cancels the active transfer (no resume in v1).

The browser enforces the 1 MiB outstanding-byte window before copying or queuing download data. ACKs cover bytes written to the sink; a sender that exceeds the window fails that transfer with `window-exceeded`.

For `purpose:"drop"`, `drop:{x:u16,y:u16,generation:u32,viewportRevision:u32}` is required and identical throughout the batch; other purposes omit it. The sender offers every file first, sends bytes/DONE sequentially, then waits for all final RESULTs. For batches below 10 MiB aggregate, the host starts OLE once all descriptors arrive; reads wait for each verified cache file. Batches of 10 MiB or more first prepare and verify every file, then commit OLE to the original point and native window identity/bounds. Preview cleanup completes before preparation input monitoring starts. Session/view/desktop changes, target movement/foreground changes, or intervening remote keys/buttons/wheel/text/paste cancel preparation; ordinary pointer movement is allowed. Temporary native hooks observe local key/button/wheel categories throughout preparation, while movement/releases are ignored; hook readiness and pump health are required. Same-window passive DOM changes cannot be proven by this window/input binding. The browser shows ACKed sending progress and Cancel while preparing; success still requires the native RESULT. A DONE ACK acknowledges staging only. RESULT ok means verified contents and accepted copy disposition, including `EndOperation` when the target uses asynchronous extraction; it does not prove a website's later cloud upload completed. Native-drop cache is bounded to 2 GiB and 16 batches across active and retained batches, including zero-byte batches. Transfers cancel after 120 seconds without progress or 60 minutes overall; retained app references have a separate five-minute post-completion limit. Cancellation aborts further reads; files already copied by the destination remain under that app's control.

Error and cancel codes (RESULT `error`, CANCEL `reason`): `hash-mismatch`, `size-mismatch`, `seq-gap` (DATA out of order), `unexpected` (packet not valid in the current state), `too-large`, `too-many`, `busy` (a transfer in that direction is already running), `write-failed`, `unreadable`, `unknown-offer` (REQUEST for an id not in the current offer), `timeout`, `user`.

Shared fixture: [`file-transfer-contract.json`](../../fixtures/file-transfer-contract.json) (header vectors, accepted/rejected packets, limits, sockets, receiver sessions, signaling) — tested from Rust, JS and Kotlin. Where this page and the fixture differ, the fixture wins.

