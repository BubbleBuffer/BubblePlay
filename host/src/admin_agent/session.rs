//! Per-connection state of the admin input agent: hello/version handshake,
//! message validation, rate limiting and tracking of held keys/buttons so a
//! vanished client never leaves input stuck down in an elevated window.
//! Pure logic; the Win32 injector is supplied by the agent binary.

use std::collections::BTreeSet;
use std::time::Instant;

use super::protocol::{
    negotiate, AgentMessage, ClientMessage, Record, RefuseCode, Role, KEYEVENTF_EXTENDEDKEY,
    KEYEVENTF_KEYUP, KEYEVENTF_SCANCODE, KEYEVENTF_UNICODE, MOUSEEVENTF_LEFTDOWN,
    MOUSEEVENTF_LEFTUP, MOUSEEVENTF_MIDDLEDOWN, MOUSEEVENTF_MIDDLEUP, MOUSEEVENTF_RIGHTDOWN,
    MOUSEEVENTF_RIGHTUP, MOUSEEVENTF_XDOWN, MOUSEEVENTF_XUP, PROTOCOL_MAX, PROTOCOL_MIN,
};

/// Frames per second a client may send (mouse at 1 kHz fits comfortably).
pub const FRAME_RATE: f64 = 2_000.0;
pub const FRAME_BURST: f64 = 2_000.0;
/// Input records per second (a 512-byte text packet is ~1 000 records).
pub const RECORD_RATE: f64 = 8_192.0;
pub const RECORD_BURST: f64 = 8_192.0;

/// Performs the actual `SendInput`; returns (records applied, last error).
pub trait Injector {
    fn inject(&mut self, records: &[Record]) -> (u32, i32);
    /// Production checks protected policy on each request; fake injectors
    /// remain pure for offline tests.
    fn policy_allowed(&self) -> bool {
        true
    }
    /// Cleanup receives only releases generated from this session's applied
    /// records. It must remain possible after permission is withdrawn.
    fn release_held(&mut self, records: &[Record]) -> (u32, i32) {
        self.inject(records)
    }
}

/// Token bucket.
#[derive(Debug, Clone)]
pub struct RateLimiter {
    capacity: f64,
    tokens: f64,
    per_second: f64,
    last: Instant,
}

impl RateLimiter {
    pub fn new(capacity: f64, per_second: f64, now: Instant) -> Self {
        Self {
            capacity,
            tokens: capacity,
            per_second,
            last: now,
        }
    }

    pub fn take(&mut self, amount: f64, now: Instant) -> bool {
        let elapsed = now.saturating_duration_since(self.last).as_secs_f64();
        self.last = now;
        self.tokens = (self.tokens + elapsed * self.per_second).min(self.capacity);
        if self.tokens >= amount {
            self.tokens -= amount;
            true
        } else {
            false
        }
    }
}

const BUTTONS: [(u32, u32, u32); 3] = [
    (1, MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP),
    (2, MOUSEEVENTF_RIGHTDOWN, MOUSEEVENTF_RIGHTUP),
    (4, MOUSEEVENTF_MIDDLEDOWN, MOUSEEVENTF_MIDDLEUP),
];

/// Keys and buttons this connection pressed and has not released.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct HeldInput {
    /// (vk, scan, SCANCODE|EXTENDEDKEY flags) of each held key.
    keys: BTreeSet<(u16, u16, u32)>,
    /// Bits: 1 left, 2 right, 4 middle, 8 X1, 16 X2.
    buttons: u32,
}

impl HeldInput {
    pub fn observe(&mut self, record: &Record) {
        match *record {
            Record::Key { vk, scan, flags } => {
                if flags & KEYEVENTF_UNICODE != 0 {
                    return;
                }
                let id = (
                    vk,
                    scan,
                    flags & (KEYEVENTF_SCANCODE | KEYEVENTF_EXTENDEDKEY),
                );
                if flags & KEYEVENTF_KEYUP != 0 {
                    self.keys.remove(&id);
                } else {
                    self.keys.insert(id);
                }
            }
            Record::Mouse { data, flags, .. } => {
                for (bit, down, up) in BUTTONS {
                    if flags & down != 0 {
                        self.buttons |= bit;
                    }
                    if flags & up != 0 {
                        self.buttons &= !bit;
                    }
                }
                let xbit = if data == 1 { 8 } else { 16 };
                if flags & MOUSEEVENTF_XDOWN != 0 {
                    self.buttons |= xbit;
                }
                if flags & MOUSEEVENTF_XUP != 0 {
                    self.buttons &= !xbit;
                }
            }
        }
    }

    pub fn is_empty(&self) -> bool {
        self.keys.is_empty() && self.buttons == 0
    }

    /// Records releasing everything held; clears the set.
    pub fn take_release(&mut self) -> Vec<Record> {
        let mut records: Vec<Record> = std::mem::take(&mut self.keys)
            .into_iter()
            .map(|(vk, scan, flags)| Record::Key {
                vk,
                scan,
                flags: flags | KEYEVENTF_KEYUP,
            })
            .collect();
        let buttons = std::mem::take(&mut self.buttons);
        for (bit, _, up) in BUTTONS {
            if buttons & bit != 0 {
                records.push(Record::Mouse {
                    dx: 0,
                    dy: 0,
                    data: 0,
                    flags: up,
                });
            }
        }
        for (bit, data) in [(8, 1), (16, 2)] {
            if buttons & bit != 0 {
                records.push(Record::Mouse {
                    dx: 0,
                    dy: 0,
                    data,
                    flags: MOUSEEVENTF_XUP,
                });
            }
        }
        records
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    AwaitHello,
    Ready { version: u16, role: Role },
}

/// What the connection loop should do after a frame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    Reply(AgentMessage),
    /// Send the reply, then disconnect.
    ReplyAndClose(AgentMessage),
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct SessionStats {
    pub frames: u64,
    pub records: u64,
    pub applied: u64,
    pub rate_limited: u64,
}

pub struct Session<I: Injector> {
    injector: I,
    state: State,
    frames: RateLimiter,
    records: RateLimiter,
    held: HeldInput,
    elevated: bool,
    pid: u32,
    /// Protocol versions this agent speaks.
    range: (u16, u16),
    pub stats: SessionStats,
}

impl<I: Injector> Session<I> {
    pub fn new(injector: I, elevated: bool, pid: u32, now: Instant) -> Self {
        Self::with_range(injector, elevated, pid, now, (PROTOCOL_MIN, PROTOCOL_MAX))
    }

    /// `range` other than the build's own is only used by the non-elevated
    /// dev agent to emulate a newer or older agent in tests.
    pub fn with_range(
        injector: I,
        elevated: bool,
        pid: u32,
        now: Instant,
        range: (u16, u16),
    ) -> Self {
        Self {
            injector,
            state: State::AwaitHello,
            frames: RateLimiter::new(FRAME_BURST, FRAME_RATE, now),
            records: RateLimiter::new(RECORD_BURST, RECORD_RATE, now),
            held: HeldInput::default(),
            elevated,
            pid,
            range,
            stats: SessionStats::default(),
        }
    }

    /// Negotiated version and client role once the hello succeeded.
    pub fn negotiated(&self) -> Option<(u16, Role)> {
        match self.state {
            State::AwaitHello => None,
            State::Ready { version, role } => Some((version, role)),
        }
    }

    fn refuse(&self, code: RefuseCode) -> AgentMessage {
        AgentMessage::Refused {
            code,
            agent_min: self.range.0,
            agent_max: self.range.1,
        }
    }

    pub fn on_frame(&mut self, kind: u8, payload: &[u8], now: Instant) -> Outcome {
        self.stats.frames += 1;
        if !self.injector.policy_allowed() {
            return Outcome::ReplyAndClose(self.refuse(RefuseCode::Invalid));
        }
        let message = match ClientMessage::decode(kind, payload) {
            Ok(message) => message,
            Err(_) => return Outcome::ReplyAndClose(self.refuse(RefuseCode::Invalid)),
        };
        let State::Ready { role, .. } = self.state else {
            return match message {
                ClientMessage::Hello { min, max, role } => {
                    match negotiate(min, max, self.range.0, self.range.1) {
                        Some(version) => {
                            self.state = State::Ready { version, role };
                            Outcome::Reply(AgentMessage::Welcome {
                                version,
                                agent_min: self.range.0,
                                agent_max: self.range.1,
                                agent_pid: self.pid,
                                elevated: self.elevated,
                            })
                        }
                        None => Outcome::ReplyAndClose(self.refuse(RefuseCode::Version)),
                    }
                }
                _ => Outcome::ReplyAndClose(self.refuse(RefuseCode::ExpectedHello)),
            };
        };
        match message {
            ClientMessage::Hello { .. } => Outcome::ReplyAndClose(self.refuse(RefuseCode::Invalid)),
            ClientMessage::Ping => Outcome::Reply(AgentMessage::Pong),
            ClientMessage::Inject(_) if role == Role::Probe => {
                Outcome::ReplyAndClose(self.refuse(RefuseCode::Invalid))
            }
            ClientMessage::Inject(records) => {
                // Both buckets are charged only when both admit the frame.
                let mut frames = self.frames.clone();
                let mut budget = self.records.clone();
                if !(frames.take(1.0, now) && budget.take(records.len() as f64, now)) {
                    // Keep elapsed-time refill but do not charge a refused frame.
                    self.frames.take(0.0, now);
                    self.records.take(0.0, now);
                    self.stats.rate_limited += 1;
                    return Outcome::Reply(self.refuse(RefuseCode::RateLimited));
                }
                self.frames = frames;
                self.records = budget;
                let (applied, error) = self.injector.inject(&records);
                let applied = (applied as usize).min(records.len());
                for record in &records[..applied] {
                    self.held.observe(record);
                }
                self.stats.records += records.len() as u64;
                self.stats.applied += applied as u64;
                Outcome::Reply(AgentMessage::Injected {
                    requested: records.len() as u16,
                    applied: applied as u16,
                    error,
                })
            }
        }
    }

    /// Release anything this client left pressed. Returns records injected.
    pub fn release_held(&mut self) -> usize {
        if self.held.is_empty() {
            return 0;
        }
        let records = self.held.take_release();
        let (applied, _) = self.injector.release_held(&records);
        applied as usize
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    #[test]
    fn revoked_policy_refuses_requests_but_releases_only_applied_held_input() {
        use std::{cell::Cell, rc::Rc};
        struct Guarded {
            allowed: Rc<Cell<bool>>,
            applied: Rc<Cell<usize>>,
            released: Rc<Cell<usize>>,
        }
        impl Injector for Guarded {
            fn inject(&mut self, records: &[Record]) -> (u32, i32) {
                assert!(self.allowed.get());
                self.applied.set(self.applied.get() + records.len());
                (records.len() as u32, 0)
            }
            fn policy_allowed(&self) -> bool {
                self.allowed.get()
            }
            fn release_held(&mut self, records: &[Record]) -> (u32, i32) {
                assert!(records.iter().all(|r| matches!(r, Record::Key { flags, .. }
                    if flags & KEYEVENTF_KEYUP != 0)));
                self.released.set(self.released.get() + records.len());
                (records.len() as u32, 0)
            }
        }
        let allowed = Rc::new(Cell::new(true));
        let applied = Rc::new(Cell::new(0));
        let released = Rc::new(Cell::new(0));
        let mut session = Session::new(
            Guarded {
                allowed: allowed.clone(),
                applied: applied.clone(),
                released: released.clone(),
            },
            true,
            42,
            Instant::now(),
        );
        let (kind, payload) = hello(1, 1, Role::Host);
        assert!(matches!(
            session.on_frame(kind, &payload, Instant::now()),
            Outcome::Reply(_)
        ));
        let (kind, payload) = inject(vec![Record::Key {
            vk: 0x41,
            scan: 0,
            flags: 0,
        }]);
        session.on_frame(kind, &payload, Instant::now());
        assert_eq!(applied.get(), 1);
        allowed.set(false);
        assert!(matches!(
            session.on_frame(kind, &payload, Instant::now()),
            Outcome::ReplyAndClose(_)
        ));
        assert_eq!(applied.get(), 1);
        assert_eq!(session.release_held(), 1);
        assert_eq!(released.get(), 1);
        assert_eq!(session.release_held(), 0);
        let (kind, payload) = frame(&ClientMessage::Ping);
        assert!(matches!(
            session.on_frame(kind, &payload, Instant::now()),
            Outcome::ReplyAndClose(_)
        ));
    }

    use super::super::protocol::{
        ClientMessage, KIND_HELLO, KIND_INJECT, KIND_PING, MOUSEEVENTF_MOVE,
    };
    use super::*;

    #[derive(Default)]
    struct Recorder {
        sent: Vec<Vec<Record>>,
        apply_limit: Option<u32>,
    }

    impl Injector for &mut Recorder {
        fn inject(&mut self, records: &[Record]) -> (u32, i32) {
            self.sent.push(records.to_vec());
            let applied = self.apply_limit.map_or(records.len() as u32, |limit| {
                limit.min(records.len() as u32)
            });
            (applied, 0)
        }
    }

    fn frame(message: &ClientMessage) -> (u8, Vec<u8>) {
        message.encode()
    }

    fn hello(min: u16, max: u16, role: Role) -> (u8, Vec<u8>) {
        frame(&ClientMessage::Hello { min, max, role })
    }

    fn inject(records: Vec<Record>) -> (u8, Vec<u8>) {
        frame(&ClientMessage::Inject(records))
    }

    const MOVE: Record = Record::Mouse {
        dx: 1,
        dy: 0,
        data: 0,
        flags: MOUSEEVENTF_MOVE,
    };

    #[test]
    fn hello_is_required_first_and_versions_are_negotiated() {
        let now = Instant::now();
        let mut recorder = Recorder::default();
        let mut session = Session::new(&mut recorder, true, 42, now);
        let (kind, payload) = inject(vec![MOVE]);
        assert_eq!(
            session.on_frame(kind, &payload, now),
            Outcome::ReplyAndClose(AgentMessage::Refused {
                code: RefuseCode::ExpectedHello,
                agent_min: PROTOCOL_MIN,
                agent_max: PROTOCOL_MAX
            })
        );
        drop(session);
        assert!(recorder.sent.is_empty());

        // A newer host that still speaks our version negotiates down.
        let mut session = Session::new(&mut recorder, true, 42, now);
        let (kind, payload) = hello(PROTOCOL_MIN, PROTOCOL_MAX + 3, Role::Host);
        assert_eq!(
            session.on_frame(kind, &payload, now),
            Outcome::Reply(AgentMessage::Welcome {
                version: PROTOCOL_MAX,
                agent_min: PROTOCOL_MIN,
                agent_max: PROTOCOL_MAX,
                agent_pid: 42,
                elevated: true
            })
        );
        assert_eq!(session.negotiated(), Some((PROTOCOL_MAX, Role::Host)));
        // A second hello is a protocol violation.
        let (kind, payload) = hello(1, 1, Role::Host);
        assert!(matches!(
            session.on_frame(kind, &payload, now),
            Outcome::ReplyAndClose(AgentMessage::Refused {
                code: RefuseCode::Invalid,
                ..
            })
        ));

        // A host that needs a newer agent is refused with the agent's range.
        let mut session = Session::new(&mut recorder, true, 42, now);
        let (kind, payload) = hello(PROTOCOL_MAX + 1, PROTOCOL_MAX + 2, Role::Host);
        assert_eq!(
            session.on_frame(kind, &payload, now),
            Outcome::ReplyAndClose(AgentMessage::Refused {
                code: RefuseCode::Version,
                agent_min: PROTOCOL_MIN,
                agent_max: PROTOCOL_MAX
            })
        );
        assert_eq!(session.negotiated(), None);
    }

    #[test]
    fn malformed_frames_close_and_probes_cannot_inject() {
        let now = Instant::now();
        let mut recorder = Recorder::default();
        let mut session = Session::new(&mut recorder, false, 1, now);
        assert!(matches!(
            session.on_frame(KIND_HELLO, &[1, 0], now),
            Outcome::ReplyAndClose(AgentMessage::Refused {
                code: RefuseCode::Invalid,
                ..
            })
        ));
        let mut session = Session::new(&mut recorder, false, 1, now);
        let (kind, payload) = hello(1, 1, Role::Probe);
        assert!(matches!(
            session.on_frame(kind, &payload, now),
            Outcome::Reply(AgentMessage::Welcome { .. })
        ));
        assert_eq!(
            session.on_frame(KIND_PING, &[], now),
            Outcome::Reply(AgentMessage::Pong)
        );
        let (kind, payload) = inject(vec![MOVE]);
        assert!(matches!(
            session.on_frame(kind, &payload, now),
            Outcome::ReplyAndClose(AgentMessage::Refused {
                code: RefuseCode::Invalid,
                ..
            })
        ));
        // Records failing validation close the connection unapplied.
        let mut session = Session::new(&mut recorder, false, 1, now);
        let (kind, payload) = hello(1, 1, Role::Host);
        session.on_frame(kind, &payload, now);
        let (_, mut bad) = inject(vec![MOVE]);
        bad[2 + 13] = 0xFF; // flags byte -> unknown flags
        assert!(matches!(
            session.on_frame(KIND_INJECT, &bad, now),
            Outcome::ReplyAndClose(AgentMessage::Refused {
                code: RefuseCode::Invalid,
                ..
            })
        ));
        drop(session);
        assert!(recorder.sent.is_empty());
    }

    #[test]
    fn injection_reports_counts_and_rate_limits() {
        let now = Instant::now();
        let mut recorder = Recorder::default();
        let mut session = Session::new(&mut recorder, true, 1, now);
        let (kind, payload) = hello(1, 1, Role::Host);
        session.on_frame(kind, &payload, now);
        let (kind, payload) = inject(vec![MOVE, MOVE]);
        assert_eq!(
            session.on_frame(kind, &payload, now),
            Outcome::Reply(AgentMessage::Injected {
                requested: 2,
                applied: 2,
                error: 0
            })
        );
        // Exhaust the record budget with full frames at a single instant.
        let full = inject(vec![MOVE; 1024]);
        let mut limited = false;
        for _ in 0..20 {
            if let Outcome::Reply(AgentMessage::Refused {
                code: RefuseCode::RateLimited,
                ..
            }) = session.on_frame(full.0, &full.1, now)
            {
                limited = true;
                break;
            }
        }
        assert!(limited);
        assert_eq!(session.stats.rate_limited, 1);
        // The connection stays usable and the budget refills over time.
        let later = now + Duration::from_secs(1);
        assert!(matches!(
            session.on_frame(full.0, &full.1, later),
            Outcome::Reply(AgentMessage::Injected {
                requested: 1024,
                ..
            })
        ));
    }

    #[test]
    fn held_keys_and_buttons_are_released_once() {
        let now = Instant::now();
        let mut recorder = Recorder::default();
        let mut session = Session::new(&mut recorder, true, 1, now);
        let (kind, payload) = hello(1, 1, Role::Host);
        session.on_frame(kind, &payload, now);
        let ctrl = Record::Key {
            vk: 0,
            scan: 0x1D,
            flags: KEYEVENTF_SCANCODE,
        };
        let a_down = Record::Key {
            vk: 0x41,
            scan: 0,
            flags: 0,
        };
        let a_up = Record::Key {
            vk: 0x41,
            scan: 0,
            flags: KEYEVENTF_KEYUP,
        };
        let text = Record::Key {
            vk: 0,
            scan: 0x263A,
            flags: KEYEVENTF_UNICODE,
        };
        let left = Record::Mouse {
            dx: 0,
            dy: 0,
            data: 0,
            flags: MOUSEEVENTF_LEFTDOWN,
        };
        let x2 = Record::Mouse {
            dx: 0,
            dy: 0,
            data: 2,
            flags: MOUSEEVENTF_XDOWN,
        };
        let (kind, payload) = inject(vec![ctrl, a_down, a_up, text, left, x2]);
        session.on_frame(kind, &payload, now);
        assert_eq!(session.release_held(), 3);
        assert_eq!(session.release_held(), 0);
        drop(session);
        assert_eq!(
            recorder.sent.last().unwrap(),
            &vec![
                Record::Key {
                    vk: 0,
                    scan: 0x1D,
                    flags: KEYEVENTF_SCANCODE | KEYEVENTF_KEYUP
                },
                Record::Mouse {
                    dx: 0,
                    dy: 0,
                    data: 0,
                    flags: MOUSEEVENTF_LEFTUP
                },
                Record::Mouse {
                    dx: 0,
                    dy: 0,
                    data: 2,
                    flags: MOUSEEVENTF_XUP
                },
            ]
        );
    }

    #[test]
    fn only_applied_records_count_as_held() {
        let now = Instant::now();
        let mut recorder = Recorder {
            apply_limit: Some(1),
            ..Default::default()
        };
        let mut session = Session::new(&mut recorder, true, 1, now);
        let (kind, payload) = hello(1, 1, Role::Host);
        session.on_frame(kind, &payload, now);
        let down = |vk| Record::Key {
            vk,
            scan: 0,
            flags: 0,
        };
        let (kind, payload) = inject(vec![down(0x41), down(0x42)]);
        assert_eq!(
            session.on_frame(kind, &payload, now),
            Outcome::Reply(AgentMessage::Injected {
                requested: 2,
                applied: 1,
                error: 0
            })
        );
        assert_eq!(session.release_held(), 1);
        drop(session);
        assert_eq!(
            recorder.sent.last().unwrap(),
            &vec![Record::Key {
                vk: 0x41,
                scan: 0,
                flags: KEYEVENTF_KEYUP
            }]
        );
    }
}
