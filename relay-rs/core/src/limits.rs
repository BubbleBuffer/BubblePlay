//! Wire size limits enforced by the relay.
//!
//! Every value is taken from `relay/server.go` so the adapters and the
//! validators agree on one number.

/// Read limit of a signaling socket (`signalLimit`): 512 KiB of text.
pub const SIGNAL_LIMIT: usize = 512 * 1024;

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

/// The shortest bearer secret an adapter accepts (RELAY_TOKEN, ADMIN_TOKEN, TURN secret), in bytes.
pub const MIN_SECRET_BYTES: usize = 32;

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

/// Largest clipboard payload the relay accepts, in raw bytes (32 KiB of text
/// per 6 bytes of UTF-8 plus a 256-byte header allowance).
pub const CLIPBOARD_RAW_MAX: usize = 32 * 1024 * 6 + 256;

/// Length of the AEAD tag the endpoints add to a clipboard payload.
pub const AEAD_TAG_BYTES: usize = 16;

/// Largest `data` field of a `ctl` message, in base64url characters: the
/// unpadded base64url length of the clipboard cap plus the AEAD tag, rounded up
/// to the next KiB. Computed from the named constants above, never a literal.
pub const CTL_DATA_MAX: usize = {
    let raw = CLIPBOARD_RAW_MAX + AEAD_TAG_BYTES;
    // Unpadded base64 of n bytes is ceil(4n / 3) characters.
    let encoded = (raw * 4).div_ceil(3);
    encoded.div_ceil(1024) * 1024
};

// A ctl message must fit in a signalling frame.
const _: () = assert!(CTL_DATA_MAX < SIGNAL_LIMIT);

/// Largest auth-hello, auth-host message (phase 2 handshake).
pub const AUTH_MAX: usize = 2 * 1024;

/// Largest auth-finish, auth-bind, pair-proof, pair-done or pair-denied message.
pub const PAIR_MAX: usize = 1024;

/// Largest admit message: 256 viewer IDs and the flag, in 16 KiB.
pub const ADMIT_MAX: usize = 16 * 1024;

/// Most viewer IDs one admit message may list.
pub const ADMIT_VIEWERS_MAX: usize = 256;

/// Largest integer a ctl `seq` may carry (2^53 - 1, the float64 safe range).
pub const CTL_SEQ_MAX: f64 = 9_007_199_254_740_991.0;

/// Pairing attempts a client prefix may fail in one window before it is refused.
pub const PAIR_PREFIX_FAILURES: u32 = 5;

/// Pairing failures against one open window before the window is dropped.
pub const PAIR_PC_FAILURES: u32 = 20;

/// The most bytes a Durable Object socket attachment may hold (the platform's
/// limit on serialized `serializeAttachment` data). Snapshots must fit.
pub const ATTACHMENT_MAX: usize = 2048;
