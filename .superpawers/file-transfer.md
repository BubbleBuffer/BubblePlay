# File transfer — intent

Status: proposed 2026-10-01 (owner asked for "native and quick" transfer via phone and web; design by the main session, not yet confirmed in detail).

## What it should feel like

- **To the PC (upload)**
  - Web: drop files on the stream, or paste (Ctrl+V) when the local clipboard holds files or an image.
  - Android: share sheet target "BubblePlay" from any app, and an Attach button in the quick bar (system file picker).
  - The host saves to `%USERPROFILE%\Downloads\BubblePlay\`, then puts the saved files on the Windows clipboard as a file list (CF_HDROP). A paste-initiated upload also injects Ctrl+V, so a screenshot pasted in the browser lands in the focused remote app. Drop/share uploads do not inject a paste; the client shows "Saved · on the PC clipboard".
- **From the PC (download)**
  - Copying files in Explorer (Ctrl+C) or Send to → BubblePlay offers them to the connected client. The client shows a chip "2 files from PC · Download". Nothing leaves the PC until the user accepts on the client.
  - Web: browser download. Android: saved to Downloads via MediaStore, notification with Open/Share.
  - No remote browsing of the PC's file system.

## Decisions

- 2026-10-01: Bytes travel on a dedicated on-demand WebSocket (`/file-ws` client, `/host-file-ws` host) relayed by the Worker, so transfers never head-of-line-block input (`/ws`) or video (`/media-ws`). Same gate cookie / host token rules as the other sockets. No auth weakening.
- 2026-10-01: The host caps file-transfer send rate so video keeps priority (use the v4 capacity estimate when a media session is active: files get what video does not use, at least 10 %).
- 2026-10-01: Downloads are offer-based only (clipboard file copy or Send To on the PC). Reason: a stolen session must not be able to read arbitrary files.
- 2026-10-01: Uploads are never opened or executed; they get a Mark-of-the-Web (`Zone.Identifier`, ZoneId=3); names are sanitised (no path separators, reserved names, trailing dots/spaces; collisions get " (n)").

## Wire contract (v1)

Capability: host capabilities gain `fileTransfer: 1`. Clients hide all file UI when it is absent.

Signaling (`/ws`, JSON):
- host → client `{"type":"file-offer","protocol":1,"offerId":u32,"items":[{"id":u32,"name":string,"size":u64}],"source":"clipboard"|"sendto"}`; a new offer replaces the previous one; `items: []` withdraws it.

File socket: `/file-ws?epoch=<mediaEpoch>` (client, gate cookie) and `/host-file-ws?epoch=<mediaEpoch>` (host, `Authorization: Bearer <host gate token>`); the Stream Lab relay serves `/lab-file-ws` and `/lab-host-file-ws` with `run` and `compat` as on its media sockets. The relay pairs one file-client with one file-host per epoch.

Text messages on the file socket (JSON):
- first message from either side `{"type":"hello","role":"file-client"|"file-host"}`; relay answers `{"type":"hello-ok"}` then `{"type":"file-peer","present":bool}`, and sends `file-peer` again when the other side arrives. Send OFFER/REQUEST only while the peer is present.
- `{"type":"ping"}` every 15 s while open; the relay answers `{"type":"pong"}` and never relays it.
- When one side closes, the relay closes the other with 1001 `file-peer-left`; a binary packet that fails validation closes the sender with 1008. REQUEST is accepted only from the file-client.
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
| 1 | OFFER | sender → receiver | UTF-8 JSON `{name,size,mime,mtimeMs,purpose:"save"|"paste",batchId,batchIndex,batchCount}` |
| 2 | ACCEPT | receiver → sender | empty |
| 3 | DATA | sender → receiver | raw bytes, ≤ 65536 |
| 4 | ACK | receiver → sender | u64 LE total bytes received |
| 5 | DONE | sender → receiver | 32-byte SHA-256 of the file |
| 6 | CANCEL | either | UTF-8 JSON `{reason}` |
| 7 | RESULT | receiver → sender | UTF-8 JSON `{ok,error?}` |
| 8 | REQUEST | client → host | UTF-8 JSON `{offerId,id}`; host answers with OFFER (same transferId) |

Rules: sender keeps ≤ 1 MiB un-ACKed; receiver ACKs at least every 256 KiB or 100 ms; receiver verifies size and SHA-256 before RESULT ok and deletes partial files on failure or CANCEL; one transfer at a time per direction, others queue; limits 2 GiB per file, 64 files per batch, names ≤ 1024 UTF-8 bytes, control payloads ≤ 4096 bytes; the socket closes after 60 s idle and is reopened on demand; a socket drop cancels the active transfer (no resume in v1).

Error and cancel codes (RESULT `error`, CANCEL `reason`): `hash-mismatch`, `size-mismatch`, `seq-gap` (DATA out of order), `unexpected` (packet not valid in the current state), `too-large`, `too-many`, `busy` (a transfer in that direction is already running), `write-failed`, `unreadable`, `unknown-offer` (REQUEST for an id not in the current offer), `timeout`, `user`.

Shared fixture: `fixtures/file-transfer-contract.json` (header vectors, accepted/rejected packets, limits, sockets, receiver sessions, signaling) — tested from Rust, JS and Kotlin. Where this page and the fixture differ, the fixture wins.

## Current state (2026-10-01)

Built on the desktop host, Worker relay and web receiver; Android is a separate track. Fixture unchanged since bf31c1b.

- Host (`host/src/file_service.rs`): enabled when the host gate token is set (not in the privileged agent); advertises `fileTransfer: 1`. Saves to `%USERPROFILE%\Downloads\BubblePlay` (`BUBBLEPLAY_DOWNLOADS_DIR` overrides) through a hidden `.bppartial` file carrying MOTW, renamed on a verified DONE; any failure or CANCEL deletes it. Each finished batch goes on the PC clipboard as CF_HDROP; for `purpose:"paste"` the host also presses Ctrl+V (only while remote input is ready). Offers come from files copied in Explorer (CF_HDROP; the host's own placements are not re-offered) and from Send To requests. Send rate is capped every 250 ms from the v4 capacity estimate. `BUBBLEPLAY_FILE_CLIPBOARD=0` turns off clipboard reading/placement and paste (used by the lab).
- Send To: setup/update scripts create `SendTo\BubblePlay.lnk` → `bubbleplay-tray.exe --send-files`; the tray writes `{protocol:1,createdMs,paths}` atomically into `%LOCALAPPDATA%\BubblePlay\sendto` (`BUBBLEPLAY_SENDTO_DIR`); the host reads it, drops requests older than 10 min, skips folders. Uninstall removes the shortcut.
- Worker (`worker/src/index.js`): `/file-ws` and `/host-file-ws` per epoch, validated relay as above; the lab relay mirrors it.
- Web: `worker/public/file-client.js` holds the logic, `file-ui.js` a simple panel (offer chip "N files from PC · size · Download", rows with progress and Cancel, drop veil). Dropping files uploads them for saving; pasting files or an image uploads them for pasting. Downloads use the File System Access API when present (save picker for one file, folder picker for several), otherwise a Blob link for files ≤ 256 MiB with a message above that. ACKs follow bytes written, so a slow disk slows the PC. All file UI stays hidden without the capability, on the secure desktop and before input is ready.
- Web module API (`FileTransferClient`, an EventTarget emitting `change`): `upload(files, purpose)`, `download(items, sinkFor)`, `transfers` (rows `{id,direction,name,size,purpose,state,error,bytes,startedAt,finishedAt}`), `offer`, `setOffer(msg)`, `cancel(id)`, `dismiss(id)`, `clearFinished()`, `reset(reason)`; helpers `describeFileError`, `canSaveToDisk`, `chooseDownloadTarget`. Hooks in `receiver.js` are marked "File transfer hook (n/5)".
- Interpretations: an upload OFFER while one is running gets CANCEL `busy`; a batch whose last file failed still places the files that arrived; folders are skipped on both sides; the web Files popover has "Choose files…", so touch browsers can upload.
- Lab (`worker/tools/stream-lab-file-transfer.mjs`, loopback relay, headless receiver, 2026-10-01): 50 MiB upload 2.75 s (~152 Mbit/s), 1 B upload, 50 MiB download 5.7 s (~74 Mbit/s, rate-capped), all hashes and MOTW verified. Presented fps ~28 before, ~23 avg / 17 min during the upload (the page also generates and hashes the 50 MiB on its main thread), ~29.5 during the download.
