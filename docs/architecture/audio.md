# PC sound

The Windows host streams sound from the default playback device to an opted-in viewer. Audio is independent of desktop capture, monitor selection, video generations and input readiness. It does not capture a microphone.

## Capture and encoding

WASAPI loopback runs in shared mode on its own standard thread. The host requests 48 kHz stereo float with automatic PCM conversion and the default sample-rate conversion quality. If initialization rejects that format, the host uses the endpoint mix format, converts PCM or float samples, mixes channels to stereo and linearly resamples to 48 kHz. The default endpoint is checked about once per second; endpoint changes and invalidation reopen capture.

The Opus encoder uses the Audio application, 128 kbps VBR, stereo, 480 samples per channel per packet (10 ms), without in-band FEC or DTX. Digital silence or an absence of loopback data produces at most 200 ms of packets, then suppresses packets until sound returns. The first packet after start or a gap marks a discontinuity. Capture and encoder failures leave video running, log once, and retry at the next device check.

## Transports

The [audio fixture](../../fixtures/audio-contract.json) defines the byte-level contract and compatibility samples.

WSS uses the protocol-4 media socket. Each binary message contains one Opus packet with a 28-byte big-endian BPA1 header:

| Offset | Bytes | Field |
| --- | --- | --- |
| 0 | 4 | Magic `BPA1` |
| 4 | 1 | Version 1 |
| 5 | 1 | Flags: bit 0 is discontinuity; other bits are zero |
| 6 | 1 | Codec 1 (Opus) |
| 7 | 1 | Channels 2 |
| 8 | 4 | Sample rate 48000 |
| 12 | 4 | Packet sequence, wrapping u32 |
| 16 | 8 | Capture time in microseconds, using the video capture clock origin |
| 24 | 2 | Samples per channel, 480 |
| 26 | 2 | Reserved, zero |
| 28 | 1–1500 | Opus payload |

The viewer opts in with `audio:1` on a protocol-4 `media-control` `start`. The host remembers the opt-in for the media epoch; absent or other values do not opt in. Older viewers receive only video. The configured host advertises optional `audio:1` in `host-capabilities`; disabling audio omits the field.

Audio has its own bounded queue of about ten packets, dropping the oldest packet when full. The socket writer drains audio ahead of video. Audio consumes no video admission slots or in-flight budget and never requests a keyframe. Sends pause while the oldest unacknowledged video frame is older than one second, and resume on ACK progress. Otherwise audio does not wait for video. Queue drops, transport gaps and reconnections mark a discontinuity.

WebRTC uses an Opus track on the existing str0m peer. A sending audio media section from the viewer's receive-only offer supplies the audio Mid and Opus payload type. Audio uses its own 48 kHz media clock; video keeps its 90 kHz clock. New offers and peer resets negotiate audio again on the fresh peer.

## Session and setting

Capture starts when a viewer session opts into WSS audio or negotiates a WebRTC audio track, with the host setting enabled. It stops when the viewer leaves or the session ends. Monitor switches, secure-desktop transitions, video generation changes and in-session peer rebuilds keep capture running. Audio does not change video encoder resets or the first-frame/input gate.

Audio defaults on. The standalone host accepts `--no-audio`. The tray stores `audio` in `tray.json`, defaulting to true when absent. The checkable **Stream PC sound** menu item saves that preference and restarts the supervised host using its existing restart path, ending any connected viewer session. The supervisor passes `--no-audio` when the saved preference is false. No audio setting is written to `host.json`.

## Limits and viewer playback

The host streams the default render endpoint, not an individual application. Mix conversion supports one to eight channels, 32-bit float and 16/24/32-bit integer PCM, with a simple linear resampler. The stereo downmix omits LFE and clamps combined samples. Capture timestamps use host read time, not the device's QPC position. Any nonzero sound, including dither, keeps packets flowing. Audio and video prioritize latency separately; there is no additional A/V synchronization. The capability describes the configured setting and remains advertised during a recoverable capture failure.

Fixture and unit tests cover packet validation, conversion, silence state, encoding, queue policy and negotiation. They do not establish live device behavior, audible quality or deployed viewer interoperability. Real playback devices, device changes, UAC/lock transitions and end-to-end playback require live qualification.

Browser viewer playback: see below.

Android viewer playback: see below.
