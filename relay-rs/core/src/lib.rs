#![forbid(unsafe_code)]
//! Sans-IO core for the BubblePlay relay.
//!
//! This crate holds the parts of the relay that need no sockets, storage,
//! clocks or randomness: message validation and the wire size limits. Adapters
//! (an axum server for self-hosting, a workers-rs Durable Object on Cloudflare)
//! supply sockets, storage, clocks and randomness and call in here.
//!
//! `protocol` is a faithful port of the validators in `relay/protocol.go`; the
//! `limits` constants mirror the read and queue limits in `relay/server.go`.
//! Every validator returns `Invalid::Drop` (ignore the message, keep the
//! socket) or `Invalid::Close` (close the socket), as Go does for that case.

pub mod directory;
pub mod gate;
pub mod limits;
pub mod log;
pub mod presence;
pub mod protocol;
pub mod relay;
pub mod rng;

pub use relay::{
    Action, AdminError, ConnId, ConnPhase, ConnSnapshot, CoreConfig, MAX_PCS, PcListing, PcRecord,
    PcTable, Reject, Relay, Route, SnapshotPairWindow, SnapshotTicket, Timings, TurnConfig,
    UpgradeRequest,
};
pub use rng::RelayRng;

pub use limits::{
    ATTACHMENT_MAX, FILE_HEADER_BYTES, FILE_LIMIT, HELLO_LIMIT, MEDIA_CONTROL_MAX,
    MEDIA_HEADER_BYTES, MEDIA_LIMIT, PENDING_LIMIT, QUEUE_BYTE_LIMIT, QUEUE_FRAMES,
    SIGNAL_LIMIT,
};
pub use protocol::{
    FileKind, FilePacket, FileText, Invalid, MediaControl, Side, SignalKind, SignalMessage,
    validate_epoch, validate_file_binary, validate_file_text, validate_media_binary,
    validate_media_text, validate_signal_text,
    validate_signal_text_for_role,
};
