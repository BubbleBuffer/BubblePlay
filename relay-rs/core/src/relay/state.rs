//! Internal state: sockets, peers, rooms and the route mapping.

use bp_device_key::DeviceId;

use super::types::Route;
use crate::gate::Prefix;
use crate::protocol::Side;

/// The three socket families.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) enum Kind {
    Signal,
    Media,
    File,
}

/// The role a socket plays. Go's host and client roles are `Host` and
/// `Viewer` here; the hello accepts only the phase-2 names (see `hello_roles`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) enum Role {
    Host,
    Viewer,
    VersionProbe,
    MediaHost,
    MediaViewer,
    FileHost,
    FileViewer,
}

impl Role {
    pub(crate) fn kind(self) -> Kind {
        match self {
            Role::Host | Role::Viewer | Role::VersionProbe => Kind::Signal,
            Role::MediaHost | Role::MediaViewer => Kind::Media,
            Role::FileHost | Role::FileViewer => Kind::File,
        }
    }

    /// The other side of the same family (Go's otherRole).
    pub(crate) fn other(self) -> Option<Role> {
        match self {
            Role::Host => Some(Role::Viewer),
            Role::Viewer => Some(Role::Host),
            Role::MediaHost => Some(Role::MediaViewer),
            Role::MediaViewer => Some(Role::MediaHost),
            Role::FileHost => Some(Role::FileViewer),
            Role::FileViewer => Some(Role::FileHost),
            Role::VersionProbe => None,
        }
    }

    /// True for the PC-side sockets (Go's "-host" suffix, plus the signal host).
    pub(crate) fn is_host_side(self) -> bool {
        matches!(self, Role::Host | Role::MediaHost | Role::FileHost)
    }

    /// The side a validator should judge a message from.
    pub(crate) fn side(self) -> Side {
        if self.is_host_side() {
            Side::Host
        } else {
            Side::Viewer
        }
    }

    /// The hello `role` strings this socket accepts. The viewer roles are
    /// `viewer`, `media-viewer` and `file-viewer`; any other name is refused
    /// as an unknown role.
    pub(crate) fn hello_roles(self) -> &'static [&'static str] {
        match self {
            Role::Host => &["host"],
            Role::Viewer => &["viewer"],
            Role::VersionProbe => &["version-probe"],
            Role::MediaHost => &["media-host"],
            Role::MediaViewer => &["media-viewer"],
            Role::FileHost => &["file-host"],
            Role::FileViewer => &["file-viewer"],
        }
    }

    /// The `<kind>-peer` event name for a data socket.
    pub(crate) fn peer_event(kind: Kind) -> &'static str {
        match kind {
            Kind::Media => "media-peer",
            Kind::File => "file-peer",
            Kind::Signal => "peer-left",
        }
    }
}

impl Route {
    pub(crate) fn role(self) -> Role {
        match self {
            Route::HostWs => Role::Host,
            Route::Ws => Role::Viewer,
            Route::HostVersionWs => Role::VersionProbe,
            Route::HostMediaWs => Role::MediaHost,
            Route::MediaWs => Role::MediaViewer,
            Route::HostFileWs => Role::FileHost,
            Route::FileWs => Role::FileViewer,
        }
    }

    /// True for routes that need the adapter's viewer authorisation.
    pub(crate) fn needs_viewer(self) -> bool {
        matches!(self, Route::Ws | Route::MediaWs | Route::FileWs)
    }

    /// True for the routes a PC's host sockets use (deleted with the PC).
    pub(crate) fn is_host_socket(self) -> bool {
        matches!(
            self,
            Route::HostWs | Route::HostVersionWs | Route::HostMediaWs | Route::HostFileWs
        )
    }
}

/// Key of the socket that is the current peer for one role. Signal keys have an
/// empty epoch; data keys carry the epoch (Go's peerKey).
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) struct PeerKey {
    pub kind: Kind,
    pub epoch: String,
    pub role: Role,
    pub pc: String,
}

/// The peer key for a role. Signalling keys ignore the epoch (Go's pcPeerKey).
pub(crate) fn key_for(role: Role, epoch: &str, pc: &str) -> PeerKey {
    let kind = role.kind();
    PeerKey {
        kind,
        epoch: if kind == Kind::Signal {
            String::new()
        } else {
            epoch.to_owned()
        },
        role,
        pc: pc.to_owned(),
    }
}

/// A signalling ticket: only its SHA-256 is kept.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Ticket {
    pub hash: [u8; 32],
    pub expires_ms: u64,
}

/// Where a socket is in its handshake.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Phase {
    /// Waiting for the first hello.
    Hello,
    /// Host registration: the challenge is out and register-proof is awaited.
    Proof {
        nonce: [u8; 32],
        id: DeviceId,
        public: [u8; 32],
        token_valid: bool,
    },
    /// Viewer signalling: the challenge is out and the signed register-proof is
    /// awaited. `turn` and `pair` are the hello's fields, kept for admission.
    ViewerProof {
        nonce: [u8; 32],
        id: DeviceId,
        public: [u8; 32],
        turn: bool,
        pair: Option<[u8; 32]>,
    },
    /// Hello done: the socket forwards messages.
    Live,
}

/// One upgraded socket.
#[derive(Clone, Debug)]
pub(crate) struct Conn {
    pub route: Route,
    pub role: Role,
    /// Canonical device id; empty only for a version probe without pc.
    pub pc: String,
    /// The socket's epoch: from the upgrade for data sockets, from the hello for viewers.
    pub epoch: String,
    pub phase: Phase,
    /// Unix ms at which the phase times out (hello, challenge or idle).
    pub deadline_ms: u64,
    /// Set once a Close action was issued. A closing socket is no longer open.
    pub closing: bool,
    pub ticket: Option<Ticket>,
    /// The client's address prefix, the key for pairing rate limits.
    pub prefix: Prefix,
    /// The admitted viewer's canonical ID. Empty until a viewer proof passes.
    pub device: String,
    /// True while the viewer is in pairing mode (admitted by a pairing window).
    pub pairing: bool,
    /// The last activity seen on a live socket, in Unix ms (touch or note_activity).
    pub activity_ms: u64,
    /// The activity value the last emitted snapshot recorded. A new snapshot is
    /// emitted once `activity_ms` has moved a step past it.
    pub saved_activity_ms: u64,
}

/// A buffered viewer message, kept for the next host.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Pending {
    pub kind: String,
    pub action: String,
    pub raw: String,
}

/// Per-PC state while any socket for the PC is open (Go's room).
#[derive(Clone, Debug, Default)]
pub(crate) struct Room {
    pub current_epoch: Option<String>,
    pub pending: Vec<Pending>,
}

/// The read limit of a socket family, in bytes.
pub(crate) fn read_limit(kind: Kind) -> usize {
    match kind {
        Kind::Signal => crate::limits::SIGNAL_LIMIT,
        Kind::Media => crate::limits::MEDIA_LIMIT,
        Kind::File => crate::limits::FILE_LIMIT,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn viewer_roles_are_the_phase_two_names() {
        assert_eq!(Role::Viewer.hello_roles(), &["viewer"]);
        assert_eq!(Role::MediaViewer.hello_roles(), &["media-viewer"]);
        assert_eq!(Role::FileViewer.hello_roles(), &["file-viewer"]);
        assert_eq!(Role::MediaHost.hello_roles(), &["media-host"]);
        assert!(!Role::Viewer.hello_roles().contains(&"client"));
        assert!(!Role::MediaViewer.hello_roles().contains(&"media-client"));
        assert!(!Role::FileViewer.hello_roles().contains(&"file-client"));
    }
}
