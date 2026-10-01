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

File socket: binary messages, 16-byte header + payload.

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

Rules: sender keeps ≤ 1 MiB un-ACKed; receiver ACKs at least every 256 KiB or 100 ms; receiver verifies size and SHA-256 before RESULT ok and deletes partial files on failure or CANCEL; one transfer at a time per direction, others queue; limits 2 GiB per file, 64 files per batch; the socket closes after 60 s idle and is reopened on demand; a socket drop cancels the active transfer (no resume in v1).

Shared fixture: `fixtures/file-transfer-contract.json` (header vectors, accepted/rejected packets, limits) — tested from Rust, JS and Kotlin.
