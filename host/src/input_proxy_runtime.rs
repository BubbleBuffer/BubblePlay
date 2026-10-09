use std::io::{self, BufReader, BufWriter};

use anyhow::{bail, Context, Result};
use windows::Win32::System::RemoteDesktop::ProcessIdToSessionId;
use windows::Win32::System::Threading::GetCurrentProcessId;

use crate::desktop_session::{DesktopKind, InputDesktop};
use crate::input::{DesktopRegion, InputSink};
use crate::input_proxy_protocol::{read_request, write_response, Request, Response};

pub const REJECT_EXPECTED_HELLO: u32 = 1;
pub const REJECT_IDENTITY_MISMATCH: u32 = 2;
pub const REJECT_STALE_REQUEST: u32 = 3;
pub const REJECT_INVALID_REGION: u32 = 4;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ProxyIdentity {
    session_id: u32,
    generation: u32,
}

struct ProxyState {
    identity: ProxyIdentity,
    last_request_id: u64,
    initialized: bool,
    input: InputSink,
}

impl ProxyState {
    fn new(session_id: u32, generation: u32) -> Self {
        Self {
            identity: ProxyIdentity {
                session_id,
                generation,
            },
            last_request_id: 0,
            initialized: false,
            // The proxy runs as the interactive user on the default desktop,
            // which is where clipboard access belongs.
            input: InputSink::new().with_clipboard(true),
        }
    }

    fn handle(&mut self, request_id: u64, request: Request) -> (Response, bool) {
        if request_id == 0 || request_id <= self.last_request_id {
            return (
                Response::Rejected {
                    code: REJECT_STALE_REQUEST,
                },
                false,
            );
        }
        self.last_request_id = request_id;

        if !self.initialized {
            return match request {
                Request::Hello {
                    session_id,
                    generation,
                } if session_id == self.identity.session_id
                    && generation == self.identity.generation =>
                {
                    self.initialized = true;
                    (
                        Response::Ready {
                            session_id,
                            generation,
                            process_id: unsafe { GetCurrentProcessId() },
                        },
                        false,
                    )
                }
                Request::Hello { .. } => (
                    Response::Rejected {
                        code: REJECT_IDENTITY_MISMATCH,
                    },
                    true,
                ),
                _ => (
                    Response::Rejected {
                        code: REJECT_EXPECTED_HELLO,
                    },
                    true,
                ),
            };
        }

        match request {
            Request::Hello { .. } => (
                Response::Rejected {
                    code: REJECT_EXPECTED_HELLO,
                },
                true,
            ),
            Request::Input(packet) => {
                let outcome = self.input.handle(&packet);
                let stale_motion_dropped = self.input.take_stats().stale_motion_dropped;
                (
                    Response::Applied {
                        requested: outcome.application.requested,
                        applied: outcome.application.applied,
                        failed_calls: outcome.application.failed_calls,
                        last_error_code: outcome.application.last_error_code,
                        accepted_sequence: outcome.accepted_sequence,
                        cursor_changed: outcome.cursor_changed,
                        sync_requested: outcome.sync_requested,
                        feedback_reliable: outcome.feedback_reliable,
                        stale_motion_dropped,
                    },
                    false,
                )
            }
            Request::SetRegion {
                left,
                top,
                width,
                height,
            } => {
                if width == 0 || height == 0 {
                    return (
                        Response::Rejected {
                            code: REJECT_INVALID_REGION,
                        },
                        false,
                    );
                }
                self.input.set_absolute_region(DesktopRegion {
                    x: left,
                    y: top,
                    width,
                    height,
                });
                (Response::Ack, false)
            }
            Request::ReleaseAll => {
                let application = self.input.release_all_report();
                (
                    Response::Released {
                        requested: application.requested,
                        applied: application.applied,
                        failed_calls: application.failed_calls,
                        last_error_code: application.last_error_code,
                    },
                    false,
                )
            }
            Request::Shutdown => {
                let _ = self.input.release_all_report();
                (Response::Ack, true)
            }
            Request::PollClipboard { reset } => (
                match self.input.poll_clipboard(reset) {
                    Some(crate::clipboard::ClipboardContent::Png(png)) => {
                        Response::ClipboardPng { png }
                    }
                    Some(crate::clipboard::ClipboardContent::Text(text)) => {
                        Response::Clipboard { text: Some(text) }
                    }
                    None => Response::Clipboard { text: None },
                },
                false,
            ),
        }
    }
}

pub fn run(session_id: u32, generation: u32) -> Result<()> {
    let current_session = current_process_session_id()?;
    if current_session != session_id {
        bail!("input proxy process session mismatch: expected {session_id}, got {current_session}");
    }

    let bound = InputDesktop::open()?.bind_thread()?;
    if bound.kind() != DesktopKind::Default {
        bail!("input proxy refuses non-default input desktop");
    }
    let security = bound.security_context()?;
    if security.token_is_local_system {
        bail!("input proxy refuses LocalSystem token");
    }
    if security.token_session_id != session_id {
        bail!("input proxy token session does not match requested session");
    }

    let stdin = io::stdin();
    let stdout = io::stdout();
    let mut reader = BufReader::new(stdin.lock());
    let mut writer = BufWriter::new(stdout.lock());
    let mut state = ProxyState::new(session_id, generation);
    loop {
        let (request_id, request) = read_request(&mut reader).context("read proxy request")?;
        let (response, stop) = state.handle(request_id, request);
        write_response(&mut writer, request_id, &response).context("write proxy response")?;
        if stop {
            return Ok(());
        }
    }
}

fn current_process_session_id() -> Result<u32> {
    let mut session_id = 0u32;
    unsafe { ProcessIdToSessionId(GetCurrentProcessId(), &mut session_id) }
        .context("resolve input proxy process session")?;
    Ok(session_id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn proxy_requires_exact_hello_and_rejects_stale_requests() {
        let mut state = ProxyState::new(3, 9);
        assert_eq!(
            state.handle(1, Request::ReleaseAll),
            (
                Response::Rejected {
                    code: REJECT_EXPECTED_HELLO
                },
                true
            )
        );

        let mut state = ProxyState::new(3, 9);
        let (ready, stop) = state.handle(
            5,
            Request::Hello {
                session_id: 3,
                generation: 9,
            },
        );
        assert!(matches!(
            ready,
            Response::Ready {
                session_id: 3,
                generation: 9,
                ..
            }
        ));
        assert!(!stop);
        assert_eq!(
            state.handle(
                5,
                Request::SetRegion {
                    left: 0,
                    top: 0,
                    width: 1920,
                    height: 1080,
                }
            ),
            (
                Response::Rejected {
                    code: REJECT_STALE_REQUEST
                },
                false
            )
        );
    }
}
