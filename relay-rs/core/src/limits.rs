//! Wire size limits enforced by the relay.
//!
//! Every value is taken from `relay/server.go` so the adapters and the
//! validators agree on one number.

/// Read limit of a signaling socket (`signalLimit`): 512 KiB of text.
pub const SIGNAL_LIMIT: usize = 512 * 1024;

/// Smallest binary frame on a signaling socket.
pub const SIGNAL_BINARY_MIN: usize = 1;

/// Largest binary frame on a signaling socket (16384 bytes).
pub const SIGNAL_BINARY_MAX: usize = 16 * 1024;

/// Length of the host's media frame header (magic, version, flags, codec,
/// channels, sample rate, timestamps, ...).
pub const MEDIA_HEADER_BYTES: usize = 28;

/// Read limit of a media socket (`mediaLimit`): 8 MiB payload plus the
/// 28-byte frame header.
pub const MEDIA_LIMIT: usize = 8 * 1024 * 1024 + MEDIA_HEADER_BYTES;

/// Length of a file packet header (`BPF1`, kind, flags, reserved,
/// transferId, seq).
pub const FILE_HEADER_BYTES: usize = 16;

/// Read limit of a file socket (`fileLimit`): one 64 KiB data packet plus the
/// 16-byte packet header.
pub const FILE_LIMIT: usize = 64 * 1024 + FILE_HEADER_BYTES;

/// Hello and any other pre-authentication message: 16 KiB.
pub const HELLO_LIMIT: usize = 16 * 1024;

/// Media-control text on a media socket (`validMediaControl`).
pub const MEDIA_CONTROL_MAX: usize = 512;

/// Outbound queue depth per peer.
pub const QUEUE_FRAMES: usize = 80;

/// Outbound queue bytes per peer (`queueByteLimit`); a peer over this is
/// closed as slow.
pub const QUEUE_BYTE_LIMIT: usize = 16 * 1024 * 1024;

/// Buffered viewer-to-host messages kept for the next host; a 65th entry
/// evicts the oldest.
pub const PENDING_LIMIT: usize = 64;
