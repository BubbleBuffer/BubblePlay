//! The close reasons clients can show. No secrets, no detail an outsider could use.

/// Malformed or unexpected hello, proof or packet before the session is live.
pub const INVALID_HELLO: &str = "invalid hello";
/// A live socket sent a packet the relay does not accept.
pub const INVALID_PACKET: &str = "invalid packet";
/// The viewer's identity proof did not verify.
pub const INVALID_PROOF: &str = "invalid proof";
/// The viewer is not on the PC's admission list and offered no pairing code.
pub const NOT_PAIRED: &str = "not paired";
/// The pairing code was wrong, or no pairing window is open.
pub const PAIRING_CODE: &str = "pairing code not accepted";
/// Too many failed pairing attempts, from this network or against this PC.
pub const RATE_LIMITED: &str = "rate limited";
/// A media or file socket's ticket or epoch was not accepted.
pub const NOT_AUTHORIZED: &str = "not authorized";
/// The socket did not finish its hello or proof in time.
pub const TIMED_OUT: &str = "timed out";
/// The host removed this viewer from the admission list.
pub const ACCESS_REMOVED: &str = "access removed";
/// The site gate did not approve this browser socket.
pub const NOT_APPROVED: &str = "not approved";
/// The socket could not be restored after the relay woke from hibernation.
pub const SESSION_NOT_RESTORED: &str = "session not restored";
/// The PC was removed by the admin.
pub const PEER_REMOVED: &str = "peer removed";
/// A newer socket took the place of a pending one while the relay was full.
pub const TOO_MANY_PENDING: &str = "too many pending connections";
