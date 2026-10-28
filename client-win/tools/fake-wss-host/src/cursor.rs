//! Host -> browser cursor packets on `/ws`, in the wire layouts of
//! `CursorState::encode` and `CursorKind::encode` in `host/src/input.rs`, read
//! by `decodeCursorState` in `worker/public/input.js` and
//! `decodeCursorType` in `worker/public/cursor.js`. Kind ids are the native
//! ids of `fixtures/cursor-types.json`.
//!
//! State (128) is 8 bytes: u8 type, u16le ack, u16le x, u16le y, u8 flags.
//! The ack is 0 unless flag 1 (ACK_VALID) is set. Flag 2 is IN_BOUNDS and flag
//! 4 is VISIBLE. Kind (129) is 2 bytes: u8 type, u8 kind.

pub const TYPE_CURSOR_STATE: u8 = 128;
pub const TYPE_CURSOR_KIND: u8 = 129;
pub const ACK_VALID: u8 = 1;
pub const IN_BOUNDS: u8 = 2;
pub const VISIBLE: u8 = 4;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Packet {
    State {
        ack: Option<u16>,
        x: u16,
        y: u16,
        in_bounds: bool,
        visible: bool,
    },
    /// Native kind id, 0 (Arrow) to 14 (Hidden).
    Kind(u8),
}

impl Packet {
    pub fn encode(self) -> Vec<u8> {
        match self {
            Packet::State {
                ack,
                x,
                y,
                in_bounds,
                visible,
            } => {
                let mut data = Vec::with_capacity(8);
                data.push(TYPE_CURSOR_STATE);
                data.extend_from_slice(&ack.unwrap_or(0).to_le_bytes());
                data.extend_from_slice(&x.to_le_bytes());
                data.extend_from_slice(&y.to_le_bytes());
                data.push(
                    (u8::from(ack.is_some()) * ACK_VALID)
                        | (u8::from(in_bounds) * IN_BOUNDS)
                        | (u8::from(visible) * VISIBLE),
                );
                data
            }
            Packet::Kind(kind) => vec![TYPE_CURSOR_KIND, kind],
        }
    }
}

/// The scripted sequence sent on `/ws` after the browser's accepted `start`,
/// as (milliseconds after the start, packet). Fixed order and values, so every
/// run sends the same bytes.
pub const SCRIPT: &[(u64, Packet)] = &[
    (
        0,
        Packet::State {
            ack: None,
            x: 32768,
            y: 32768,
            in_bounds: true,
            visible: true,
        },
    ),
    (10, Packet::Kind(1)),
    (
        20,
        Packet::State {
            ack: Some(7),
            x: 0,
            y: 0,
            in_bounds: true,
            visible: true,
        },
    ),
    (30, Packet::Kind(2)),
    (
        40,
        Packet::State {
            ack: Some(0xFFFF),
            x: 0xFFFF,
            y: 0xFFFF,
            in_bounds: false,
            visible: true,
        },
    ),
    (50, Packet::Kind(14)),
    (
        60,
        Packet::State {
            ack: None,
            x: 1,
            y: 2,
            in_bounds: false,
            visible: false,
        },
    ),
    (70, Packet::Kind(0)),
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn state_layout_matches_the_host_encoder() {
        let with_ack = Packet::State {
            ack: Some(0x0102),
            x: 0x0304,
            y: 0x0506,
            in_bounds: true,
            visible: false,
        };
        assert_eq!(
            with_ack.encode(),
            vec![128, 0x02, 0x01, 0x04, 0x03, 0x06, 0x05, 3]
        );
        let no_ack = Packet::State {
            ack: None,
            x: 0,
            y: 65535,
            in_bounds: false,
            visible: true,
        };
        assert_eq!(no_ack.encode(), vec![128, 0, 0, 0, 0, 0xFF, 0xFF, 4]);
    }

    #[test]
    fn kind_layout_is_two_bytes() {
        assert_eq!(Packet::Kind(14).encode(), vec![129, 14]);
        assert_eq!(Packet::Kind(0).encode(), vec![129, 0]);
    }

    #[test]
    fn script_is_ordered_in_time() {
        let times: Vec<u64> = SCRIPT.iter().map(|(at, _)| *at).collect();
        let mut sorted = times.clone();
        sorted.sort_unstable();
        assert_eq!(times, sorted);
        assert_eq!(SCRIPT.len(), 8);
    }
}
