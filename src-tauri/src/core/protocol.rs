/// Binary protocol for sharing input events between machines.
///
/// All events are serialized with bincode for compact, low-latency transport.
/// Event layout:
///   [4 bytes: length][1 byte: event type][payload ...]

use serde::{Deserialize, Serialize};

/// Mouse button identifiers
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub enum MouseButton {
    Left,
    Right,
    Middle,
    Other(u8),
}

// Keyboard modifier flags (bitmask)
bitflags::bitflags! {
    #[derive(Debug, Clone, Copy, Serialize, Deserialize)]
    pub struct Modifiers: u8 {
        const NONE   = 0x00;
        const SHIFT  = 0x01;
        const CTRL   = 0x02;
        const ALT    = 0x04;
        const META   = 0x08; // Cmd on macOS, Win on Windows
        const CAPS   = 0x10;
    }
}

/// The core input event that gets transmitted over the network.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum InputEvent {
    /// Relative mouse movement (delta pixels)
    MouseMove {
        dx: i16,
        dy: i16,
    },

    /// Absolute mouse position (normalized 0.0–1.0)
    MouseMoveAbsolute {
        x: f32,
        y: f32,
    },

    /// Mouse button pressed
    MouseDown {
        button: MouseButton,
    },

    /// Mouse button released
    MouseUp {
        button: MouseButton,
    },

    /// Scroll wheel
    Scroll {
        dx: i16,
        dy: i16,
    },

    /// Key pressed
    KeyDown {
        keycode: u16,
        modifiers: Modifiers,
    },

    /// Key released
    KeyUp {
        keycode: u16,
        modifiers: Modifiers,
    },

    /// Clipboard content sync (deferred to v2)
    Clipboard {
        data: Vec<u8>,
    },

    /// Screen information broadcast
    ScreenInfo {
        width: u32,
        height: u32,
        dpi: u32,
        name: String,
    },

    /// Cursor has entered this machine's screen from a neighbor
    CursorEnter {
        /// Normalized position where cursor should appear
        x: f32,
        y: f32,
    },

    /// Cursor has left this machine's screen
    CursorLeave,

    /// Heartbeat / keep-alive
    Ping,

    /// Response to ping
    Pong,
}

/// Codec for encoding/decoding events on the wire.
/// Format: [4-byte big-endian length][bincode payload]
pub struct EventCodec;

impl EventCodec {
    /// Encode an event into bytes ready for the wire.
    pub fn encode(event: &InputEvent) -> anyhow::Result<Vec<u8>> {
        let payload = bincode::serialize(event)?;
        let len = (payload.len() as u32).to_be_bytes();
        let mut buf = Vec::with_capacity(4 + payload.len());
        buf.extend_from_slice(&len);
        buf.extend_from_slice(&payload);
        Ok(buf)
    }

    /// Decode one event from a byte buffer.
    /// Returns the event and the number of bytes consumed.
    /// Returns None if the buffer doesn't contain a complete event yet.
    pub fn decode(buf: &[u8]) -> anyhow::Result<Option<(InputEvent, usize)>> {
        if buf.len() < 4 {
            return Ok(None);
        }
        let len = u32::from_be_bytes([buf[0], buf[1], buf[2], buf[3]]) as usize;
        if buf.len() < 4 + len {
            return Ok(None);
        }
        let event: InputEvent = bincode::deserialize(&buf[4..4 + len])?;
        Ok(Some((event, 4 + len)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_mouse_move() {
        let event = InputEvent::MouseMove { dx: 100, dy: -50 };
        let encoded = EventCodec::encode(&event).unwrap();
        let (decoded, consumed) = EventCodec::decode(&encoded)
            .unwrap()
            .expect("should decode");

        assert_eq!(consumed, encoded.len());
        if let InputEvent::MouseMove { dx, dy } = decoded {
            assert_eq!(dx, 100);
            assert_eq!(dy, -50);
        } else {
            panic!("wrong event type");
        }
    }

    #[test]
    fn decode_partial_returns_none() {
        let event = InputEvent::MouseMove { dx: 1, dy: 2 };
        let encoded = EventCodec::encode(&event).unwrap();
        // Only feed first 2 bytes (incomplete length header)
        assert!(EventCodec::decode(&encoded[..2]).unwrap().is_none());
    }

    #[test]
    fn roundtrip_key_event() {
        let event = InputEvent::KeyDown {
            keycode: 0x24, // 'A' on macOS
            modifiers: Modifiers::SHIFT | Modifiers::META,
        };
        let encoded = EventCodec::encode(&event).unwrap();
        let (decoded, _) = EventCodec::decode(&encoded).unwrap().unwrap();

        if let InputEvent::KeyDown { keycode, modifiers } = decoded {
            assert_eq!(keycode, 0x24);
            assert!(modifiers.contains(Modifiers::SHIFT));
            assert!(modifiers.contains(Modifiers::META));
        } else {
            panic!("wrong event type");
        }
    }
}
