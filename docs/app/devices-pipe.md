# Devices pipe (host to Flutter app)

The host serves the Devices UI of the Flutter app's "This PC" section over a per-user named pipe. It carries the events and commands of `host/src/devices_ui_api.rs` (`DevicesEvent`, `DevicesCommand`) as JSON.

## Pipe

- Name: `\.\pipe\bubbleplay-devices-v1-<SID>` where `<SID>` is the string SID of the host process's user (e.g. `S-1-5-21-...`).
- Byte-mode duplex pipe, created with `FILE_FLAG_FIRST_PIPE_INSTANCE` for the first instance, remote clients rejected (`PIPE_REJECT_REMOTE_CLIENTS`), DACL granting access to the current user's SID only (SDDL `D:P(A;;GA;;;<SID>)`; no SYSTEM or Administrators entry). Several clients may be connected at once; each gets every event.
- The client checks the server too: after connecting it reads the server process id (`GetNamedPipeServerProcessId`) and requires that process's token user to equal its own SID; otherwise it closes the pipe and treats the host as not connected.

## Framing

One JSON object per line (UTF-8, `\n`). A line longer than 16 KiB, invalid UTF-8 or invalid JSON ends that client's connection. Unknown `type` values from the server are ignored by the client; unknown client commands are ignored by the server (a log line without secrets).

Pairing codes, QR URIs and comparison numbers travel on this pipe and are never logged.

## Server to client

| `type` | Fields | From |
| --- | --- | --- |
| `hello` | `protocol` (1), `pcName`, `pcId`, `relayOrigin`, `relayOnline` | first line on every connection |
| `relay` | `online` | the relay link changes |
| `list` | `devices`: `[{id, name, lastSeen (RFC 3339 or null), connected}]` | `DevicesEvent::List` |
| `pairWaiting` | `codeDisplay`, `qrUri`, `pcId`, `expiresAtMs` (Unix ms) | `PairWaiting` |
| `pairExpired`, `pairStopped`, `pairFailed`, `pairDenied` | none | the same events |
| `pairRequest` | `id`, `name`, `sasDisplay` | `PairRequest` |
| `pairDone` | `id`, `name` | `PairDone` |
| `saveError` | `message` | `SaveError` |
| `show` | none | the tray asked to show the Devices section |

On connect the server sends `hello`, then the current state: the last `list` (and issues `DevicesCommand::Open` itself so the list is fresh), the open pairing (`pairWaiting`, or `pairRequest` awaiting a decision) if there is one.

## Client to server

| `type` | Fields | Command |
| --- | --- | --- |
| `open` | none | `Open` (host answers with a fresh `list`) |
| `revoke` | `id` | `Revoke(id)` |
| `startPairing`, `cancelPairing` | none | `StartPairing`, `CancelPairing` |
| `allow`, `deny` | `id` | `Allow(id)`, `Deny(id)` |

## Tray

With the Flutter app installed beside the tray (`bubbleplay_app.exe`), the tray's "Devices" item finds the app's window and brings it to the front, or starts `bubbleplay_app.exe --section=this-pc` detached; it then writes the stdin line `show-devices-app` to the host, which sends `show` to connected pipe clients (the app switches to This PC). Without the app installed the tray writes `open-devices` as before and the Win32 Devices window opens.
