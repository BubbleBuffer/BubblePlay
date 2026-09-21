# Phone service failure — 2026-09-21 UTC / 2026-09-22 investigation

The WebRTC-only phone attempt disproved the earlier workaround as a complete
solution. Production host log at 21:52:56 records attempt e9371b66 using WebRTC,
then native H264 presentation at 21:53:00. At 21:53:02, older attempt 830eb7e3
re-registers in automatic mode and starts WSS; a session ends at 21:53:04.
This is evidence of overlapping browser sessions, not proof that all observed
restarts have one cause. Current receiver already treats policy replacement as
terminal; that recovery behavior is not deployed.

New WebRTC HEVC attempt ee260c26 starts at 21:53:06, presents only three frames
by 21:53:10, switches to game mode at 21:53:11, and logs Agent encode permit
timeout at 21:53:15.653. Thousands of immediate reconnecting encode warnings
follow. Agent reconnect succeeds at 21:53:16.168; the data channel closes at
21:53:16.578. SCM System event 7024 at local 23:53:16 reports transport service
termination with service-specific error 1. The service was verified stopped.

The active stderr file is exactly 7 MiB, matching the supervisor's retained-tail
size after its 8 MiB live budget is exceeded. Combined with the rapid warning
flood and stop event, this strongly supports log-budget shutdown. No explicit
supervisor error text survived in its combined log, so the exact stop cause is
inferred from these observations and the supervisor implementation.

Concrete code defect fixed: the privileged encode error branch logged on every
iteration and left next_admission unchanged. A disconnected Agent therefore
caused an immediate repeated failure. Recovery now sets a future admission
using the existing reconnect deadline and a 50 ms floor, and shares the existing
30-second reconnect log gate. Deadline arithmetic uses the time after encode
returns. The supervisor's log guard and the one-second encode timeout remain.

The original missing encode completion is NOT root-caused. Capture, encoder,
IPC delivery, or stale-result timing remain possible. The installed privileged
path was not covered by the earlier isolated local-capture soak. Do not claim
end-to-end phone recovery from that earlier test or these unit tests.

Evidence copies: artifacts/phone-agent-timeout-host.log and
artifacts/phone-agent-timeout-agent.log. These are bounded retained logs;
earlier events were inspected before truncation and summarized above.

An authorized restart was attempted, but unelevated SCM access was denied and
Windows cancelled the UAC launch. Service remains stopped awaiting OS elevation.
No binary or Worker deployment performed. Wiki capture unavailable; note retained
locally for later ingestion.

Validation: host checks passed (391 Rust tests and all-target checks), recorded in artifacts/phone-agent-recovery-check.txt. After the final deadline-time correction, both focused recovery tests passed in artifacts/phone-agent-recovery-focused.txt using the configured SDK stub path. No live privileged recovery validation has passed for this fix.
