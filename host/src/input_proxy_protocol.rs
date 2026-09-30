use std::io::{self, Read, Write};

pub const MAGIC: [u8; 4] = *b"BPXP";
pub const VERSION: u16 = 1;
pub const HEADER_LEN: usize = 20;
pub const MAX_PAYLOAD_BYTES: usize = 64 * 1024;
/// Responses may carry one whole clipboard text (1 MiB cap) plus framing.
pub const MAX_RESPONSE_PAYLOAD_BYTES: usize = crate::clipboard::MAX_CLIPBOARD_BYTES + 64;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Request {
    Hello {
        session_id: u32,
        generation: u32,
    },
    Input(Vec<u8>),
    SetRegion {
        left: i32,
        top: i32,
        width: u32,
        height: u32,
    },
    ReleaseAll,
    Shutdown,
    /// Ask for interactive-session clipboard text changed since the last
    /// poll. `reset` re-baselines without reporting the current contents.
    PollClipboard {
        reset: bool,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Response {
    Ready {
        session_id: u32,
        generation: u32,
        process_id: u32,
    },
    Applied {
        requested: u32,
        applied: u32,
        failed_calls: u32,
        last_error_code: Option<i32>,
        accepted_sequence: Option<u16>,
        cursor_changed: bool,
        sync_requested: bool,
        feedback_reliable: bool,
        stale_motion_dropped: u64,
    },
    Released {
        requested: u32,
        applied: u32,
        failed_calls: u32,
        last_error_code: Option<i32>,
    },
    Ack,
    Rejected {
        code: u32,
    },
    Clipboard {
        text: Option<String>,
    },
}

pub fn write_request<W: Write>(
    writer: &mut W,
    request_id: u64,
    request: &Request,
) -> io::Result<()> {
    write_frame(writer, request_id, 1, &encode_request(request))
}

pub fn read_request<R: Read>(reader: &mut R) -> io::Result<(u64, Request)> {
    let (request_id, family, payload) = read_frame(reader)?;
    if family != 1 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "input proxy frame is not a request",
        ));
    }
    decode_request(&payload).map(|request| (request_id, request))
}

pub fn write_response<W: Write>(
    writer: &mut W,
    request_id: u64,
    response: &Response,
) -> io::Result<()> {
    write_frame(writer, request_id, 2, &encode_response(response))
}

pub fn read_response<R: Read>(reader: &mut R) -> io::Result<(u64, Response)> {
    let (request_id, family, payload) = read_frame(reader)?;
    if family != 2 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "input proxy frame is not a response",
        ));
    }
    decode_response(&payload).map(|response| (request_id, response))
}

fn encode_request(request: &Request) -> Vec<u8> {
    match request {
        Request::Hello {
            session_id,
            generation,
        } => {
            let mut payload = vec![1];
            payload.extend_from_slice(&session_id.to_le_bytes());
            payload.extend_from_slice(&generation.to_le_bytes());
            payload
        }
        Request::Input(packet) => {
            let mut payload = vec![2];
            payload.extend_from_slice(packet);
            payload
        }
        Request::SetRegion {
            left,
            top,
            width,
            height,
        } => {
            let mut payload = vec![3];
            payload.extend_from_slice(&left.to_le_bytes());
            payload.extend_from_slice(&top.to_le_bytes());
            payload.extend_from_slice(&width.to_le_bytes());
            payload.extend_from_slice(&height.to_le_bytes());
            payload
        }
        Request::ReleaseAll => vec![4],
        Request::Shutdown => vec![5],
        Request::PollClipboard { reset } => vec![6, u8::from(*reset)],
    }
}

fn decode_request(payload: &[u8]) -> io::Result<Request> {
    let (&kind, body) = payload.split_first().ok_or_else(invalid_payload)?;
    match kind {
        1 if body.len() == 8 => Ok(Request::Hello {
            session_id: u32::from_le_bytes(body[..4].try_into().unwrap()),
            generation: u32::from_le_bytes(body[4..8].try_into().unwrap()),
        }),
        2 if !body.is_empty() => Ok(Request::Input(body.to_vec())),
        3 if body.len() == 16 => Ok(Request::SetRegion {
            left: i32::from_le_bytes(body[..4].try_into().unwrap()),
            top: i32::from_le_bytes(body[4..8].try_into().unwrap()),
            width: u32::from_le_bytes(body[8..12].try_into().unwrap()),
            height: u32::from_le_bytes(body[12..16].try_into().unwrap()),
        }),
        4 if body.is_empty() => Ok(Request::ReleaseAll),
        5 if body.is_empty() => Ok(Request::Shutdown),
        6 if body.len() == 1 => Ok(Request::PollClipboard {
            reset: decode_bool(body[0])?,
        }),
        _ => Err(invalid_payload()),
    }
}

fn encode_response(response: &Response) -> Vec<u8> {
    match response {
        Response::Ack => vec![3],
        Response::Clipboard { text } => {
            let mut payload = vec![6, u8::from(text.is_some())];
            if let Some(text) = text {
                payload.extend_from_slice(text.as_bytes());
            }
            payload
        }
        Response::Rejected { code } => {
            let mut payload = vec![4];
            payload.extend_from_slice(&code.to_le_bytes());
            payload
        }
        Response::Ready {
            session_id,
            generation,
            process_id,
        } => {
            let mut payload = vec![1];
            payload.extend_from_slice(&session_id.to_le_bytes());
            payload.extend_from_slice(&generation.to_le_bytes());
            payload.extend_from_slice(&process_id.to_le_bytes());
            payload
        }
        Response::Applied {
            requested,
            applied,
            failed_calls,
            last_error_code,
            accepted_sequence,
            cursor_changed,
            sync_requested,
            feedback_reliable,
            stale_motion_dropped,
        } => {
            let mut payload = vec![2];
            payload.extend_from_slice(&requested.to_le_bytes());
            payload.extend_from_slice(&applied.to_le_bytes());
            payload.extend_from_slice(&failed_calls.to_le_bytes());
            payload.extend_from_slice(&last_error_code.unwrap_or_default().to_le_bytes());
            payload.push(u8::from(last_error_code.is_some()));
            payload.extend_from_slice(&accepted_sequence.unwrap_or_default().to_le_bytes());
            payload.push(u8::from(accepted_sequence.is_some()));
            payload.push(u8::from(*cursor_changed));
            payload.push(u8::from(*sync_requested));
            payload.push(u8::from(*feedback_reliable));
            payload.extend_from_slice(&stale_motion_dropped.to_le_bytes());
            payload
        }
        Response::Released {
            requested,
            applied,
            failed_calls,
            last_error_code,
        } => {
            let mut payload = vec![5];
            payload.extend_from_slice(&requested.to_le_bytes());
            payload.extend_from_slice(&applied.to_le_bytes());
            payload.extend_from_slice(&failed_calls.to_le_bytes());
            payload.extend_from_slice(&last_error_code.unwrap_or_default().to_le_bytes());
            payload.push(u8::from(last_error_code.is_some()));
            payload
        }
    }
}

fn decode_response(payload: &[u8]) -> io::Result<Response> {
    let (&kind, body) = payload.split_first().ok_or_else(invalid_payload)?;
    match kind {
        1 if body.len() == 12 => Ok(Response::Ready {
            session_id: u32::from_le_bytes(body[..4].try_into().unwrap()),
            generation: u32::from_le_bytes(body[4..8].try_into().unwrap()),
            process_id: u32::from_le_bytes(body[8..12].try_into().unwrap()),
        }),
        2 if body.len() == 31 => {
            let has_error = decode_bool(body[16])?;
            let has_sequence = decode_bool(body[19])?;
            Ok(Response::Applied {
                requested: u32::from_le_bytes(body[..4].try_into().unwrap()),
                applied: u32::from_le_bytes(body[4..8].try_into().unwrap()),
                failed_calls: u32::from_le_bytes(body[8..12].try_into().unwrap()),
                last_error_code: has_error
                    .then(|| i32::from_le_bytes(body[12..16].try_into().unwrap())),
                accepted_sequence: has_sequence
                    .then(|| u16::from_le_bytes(body[17..19].try_into().unwrap())),
                cursor_changed: decode_bool(body[20])?,
                sync_requested: decode_bool(body[21])?,
                feedback_reliable: decode_bool(body[22])?,
                stale_motion_dropped: u64::from_le_bytes(body[23..31].try_into().unwrap()),
            })
        }
        3 if body.is_empty() => Ok(Response::Ack),
        6 if !body.is_empty() => {
            let has_text = decode_bool(body[0])?;
            let text = &body[1..];
            if !has_text {
                return if text.is_empty() {
                    Ok(Response::Clipboard { text: None })
                } else {
                    Err(invalid_payload())
                };
            }
            if text.is_empty() || text.len() > crate::clipboard::MAX_CLIPBOARD_BYTES {
                return Err(invalid_payload());
            }
            let text = String::from_utf8(text.to_vec()).map_err(|_| invalid_payload())?;
            Ok(Response::Clipboard { text: Some(text) })
        }
        4 if body.len() == 4 => Ok(Response::Rejected {
            code: u32::from_le_bytes(body.try_into().unwrap()),
        }),
        5 if body.len() == 17 => {
            let has_error = decode_bool(body[16])?;
            Ok(Response::Released {
                requested: u32::from_le_bytes(body[..4].try_into().unwrap()),
                applied: u32::from_le_bytes(body[4..8].try_into().unwrap()),
                failed_calls: u32::from_le_bytes(body[8..12].try_into().unwrap()),
                last_error_code: has_error
                    .then(|| i32::from_le_bytes(body[12..16].try_into().unwrap())),
            })
        }
        _ => Err(invalid_payload()),
    }
}

fn decode_bool(value: u8) -> io::Result<bool> {
    match value {
        0 => Ok(false),
        1 => Ok(true),
        _ => Err(invalid_payload()),
    }
}

fn invalid_payload() -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, "invalid input proxy payload")
}

fn write_frame<W: Write>(
    writer: &mut W,
    request_id: u64,
    family: u16,
    payload: &[u8],
) -> io::Result<()> {
    if payload.len() > max_payload_bytes(family) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "input proxy payload exceeds limit",
        ));
    }
    let mut header = [0u8; HEADER_LEN];
    header[..4].copy_from_slice(&MAGIC);
    header[4..6].copy_from_slice(&VERSION.to_le_bytes());
    header[6..8].copy_from_slice(&family.to_le_bytes());
    header[8..16].copy_from_slice(&request_id.to_le_bytes());
    header[16..20].copy_from_slice(&(payload.len() as u32).to_le_bytes());
    writer.write_all(&header)?;
    writer.write_all(payload)?;
    writer.flush()
}

fn read_frame<R: Read>(reader: &mut R) -> io::Result<(u64, u16, Vec<u8>)> {
    let mut header = [0u8; HEADER_LEN];
    reader.read_exact(&mut header)?;
    if header[..4] != MAGIC {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "bad input proxy magic",
        ));
    }
    if u16::from_le_bytes(header[4..6].try_into().unwrap()) != VERSION {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "unsupported input proxy version",
        ));
    }
    let family = u16::from_le_bytes(header[6..8].try_into().unwrap());
    let request_id = u64::from_le_bytes(header[8..16].try_into().unwrap());
    let payload_len = u32::from_le_bytes(header[16..20].try_into().unwrap()) as usize;
    if payload_len > max_payload_bytes(family) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "input proxy payload exceeds limit",
        ));
    }
    let mut payload = vec![0; payload_len];
    reader.read_exact(&mut payload)?;
    Ok((request_id, family, payload))
}

fn max_payload_bytes(family: u16) -> usize {
    if family == 2 {
        MAX_RESPONSE_PAYLOAD_BYTES
    } else {
        MAX_PAYLOAD_BYTES
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_proxy_message_round_trips_with_request_identity() {
        let requests = [
            Request::Hello {
                session_id: 2,
                generation: 9,
            },
            Request::Input(vec![7, 8, 9]),
            Request::SetRegion {
                left: -1920,
                top: 0,
                width: 1920,
                height: 1080,
            },
            Request::ReleaseAll,
            Request::Shutdown,
            Request::PollClipboard { reset: true },
            Request::PollClipboard { reset: false },
        ];
        for request in requests {
            let mut wire = Vec::new();
            write_request(&mut wire, 41, &request).unwrap();
            assert_eq!(read_request(&mut wire.as_slice()).unwrap(), (41, request));
        }

        let responses = [
            Response::Ready {
                session_id: 2,
                generation: 9,
                process_id: 123,
            },
            Response::Applied {
                requested: 2,
                applied: 1,
                failed_calls: 1,
                last_error_code: Some(5),
                accepted_sequence: Some(44),
                cursor_changed: true,
                sync_requested: false,
                feedback_reliable: true,
                stale_motion_dropped: 3,
            },
            Response::Ack,
            Response::Rejected { code: 7 },
            Response::Released {
                requested: 3,
                applied: 2,
                failed_calls: 1,
                last_error_code: Some(5),
            },
            Response::Clipboard { text: None },
            Response::Clipboard {
                text: Some("héllo\r\nwörld".into()),
            },
            Response::Clipboard {
                text: Some("x".repeat(crate::clipboard::MAX_CLIPBOARD_BYTES)),
            },
        ];
        for response in responses {
            let mut wire = Vec::new();
            write_response(&mut wire, 52, &response).unwrap();
            assert_eq!(read_response(&mut wire.as_slice()).unwrap(), (52, response));
        }
    }

    #[test]
    fn clipboard_responses_are_bounded_and_utf8() {
        for payload in [
            vec![6],
            vec![6, 0, b'x'],
            vec![6, 1],
            vec![6, 1, 0xFF],
            vec![6, 2, b'x'],
        ] {
            assert!(decode_response(&payload).is_err(), "{payload:?}");
        }
        let too_large = Response::Clipboard {
            text: Some("x".repeat(crate::clipboard::MAX_CLIPBOARD_BYTES + 1)),
        };
        assert!(decode_response(&encode_response(&too_large)).is_err());
        // Requests keep the small limit even though responses may be larger.
        let mut wire = Vec::new();
        assert!(write_request(&mut wire, 1, &Request::Input(vec![0; MAX_PAYLOAD_BYTES])).is_err());
    }
}
