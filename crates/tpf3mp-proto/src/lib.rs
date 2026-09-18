//! Wire protocol shared by `tpf3mp-server` and `tpf3mp-agent`.
//!
//! A control stream opens with a fixed-layout [preamble](encode_preamble) that
//! carries the protocol version. Its layout never changes, so any two releases
//! can tell each other which version they speak, however much else differs.
//! After the preamble, every message is one frame: a little-endian `u32`
//! payload length followed by a postcard-encoded [`Message`]. A frame above the
//! channel's cap is a protocol violation. Decoding enforces the bounds of every
//! field (see [`Text`]), so a decoded message is always within limits.

mod text;

use std::fmt;

use serde::{Deserialize, Serialize};
use thiserror::Error;

pub use text::{Text, TextError};

/// Protocol version. Client and server must match exactly.
pub const PROTOCOL_VERSION: u32 = 1;

/// Application protocol name negotiated during the TLS handshake.
pub const ALPN: &[u8] = b"tpf3mp";

/// Largest frame accepted on the control stream.
pub const CONTROL_MAX_FRAME: usize = 64 * 1024;

const MAGIC: [u8; 6] = *b"TPF3MP";

/// Length of the version preamble: six magic bytes and a little-endian `u32`.
pub const PREAMBLE_LEN: usize = MAGIC.len() + 4;

/// Length of a frame header: the little-endian `u32` payload length.
pub const FRAME_HEADER_LEN: usize = 4;

pub fn encode_preamble(version: u32) -> [u8; PREAMBLE_LEN] {
    let mut bytes = [0; PREAMBLE_LEN];
    bytes[..MAGIC.len()].copy_from_slice(&MAGIC);
    bytes[MAGIC.len()..].copy_from_slice(&version.to_le_bytes());
    bytes
}

pub fn decode_preamble(bytes: [u8; PREAMBLE_LEN]) -> Result<u32, PreambleError> {
    let (magic, version) = bytes.split_at(MAGIC.len());
    if magic != MAGIC {
        return Err(PreambleError::BadMagic);
    }
    let mut version_bytes = [0; 4];
    version_bytes.copy_from_slice(version);
    Ok(u32::from_le_bytes(version_bytes))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum PreambleError {
    #[error("the stream does not start with the TPF3-MP preamble")]
    BadMagic,
}

/// Every message on the control stream.
///
/// Variants are identified by their position: append new variants at the end
/// and never reorder them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Message {
    Hello(Hello),
    Welcome(Welcome),
    Reject(Reject),
}

/// The client's first message after the preamble.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Hello {
    pub client_version: Text<64>,
    pub platform: Platform,
}

/// The server's answer to an accepted [`Hello`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Welcome {
    pub server_version: Text<64>,
    pub session_id: SessionId,
}

/// The server's answer to a [`Hello`] it will not serve. The server closes the
/// connection after sending it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Reject {
    pub reason: RejectReason,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum RejectReason {
    ServerFull,
}

impl fmt::Display for RejectReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ServerFull => f.write_str("the server is full; try again later"),
        }
    }
}

/// Non-secret identifier of one connection, safe to quote in bug reports.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct SessionId(pub [u8; 16]);

impl fmt::Display for SessionId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("s-")?;
        for byte in self.0 {
            write!(f, "{byte:02x}")?;
        }
        Ok(())
    }
}

impl fmt::Debug for SessionId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(self, f)
    }
}

/// The operating system and CPU architecture a client runs on. Rooms mix
/// platforms, and the server uses this to choose a room's anchor replica.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Platform {
    pub os: Os,
    pub arch: Arch,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Os {
    Windows,
    Linux,
    MacOs,
    Other,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Arch {
    X86_64,
    Aarch64,
    Other,
}

impl Platform {
    pub fn current() -> Self {
        let os = match std::env::consts::OS {
            "windows" => Os::Windows,
            "linux" => Os::Linux,
            "macos" => Os::MacOs,
            _ => Os::Other,
        };
        let arch = match std::env::consts::ARCH {
            "x86_64" => Arch::X86_64,
            "aarch64" => Arch::Aarch64,
            _ => Arch::Other,
        };
        Self { os, arch }
    }
}

#[derive(Debug, Error)]
pub enum FrameError {
    #[error("frame of {len} bytes exceeds the {max}-byte limit")]
    TooLarge { len: usize, max: usize },
    #[error("empty frame")]
    Empty,
    #[error("malformed message: {0}")]
    Malformed(#[from] postcard::Error),
    #[error("{0} unexpected bytes after the message")]
    TrailingBytes(usize),
}

/// Encodes `message` as one frame, refusing to produce a frame the receiver
/// would reject.
pub fn encode_frame(message: &Message, max: usize) -> Result<Vec<u8>, FrameError> {
    let payload = postcard::to_stdvec(message)?;
    let too_large = FrameError::TooLarge {
        len: payload.len(),
        max,
    };
    if payload.len() > max {
        return Err(too_large);
    }
    let len = u32::try_from(payload.len()).map_err(|_| too_large)?;
    let mut frame = Vec::with_capacity(FRAME_HEADER_LEN + payload.len());
    frame.extend_from_slice(&len.to_le_bytes());
    frame.extend_from_slice(&payload);
    Ok(frame)
}

/// Reads a frame header and returns the payload length, which is at most `max`.
pub fn frame_len(header: [u8; FRAME_HEADER_LEN], max: usize) -> Result<usize, FrameError> {
    let len = usize::try_from(u32::from_le_bytes(header)).unwrap_or(usize::MAX);
    if len == 0 {
        return Err(FrameError::Empty);
    }
    if len > max {
        return Err(FrameError::TooLarge { len, max });
    }
    Ok(len)
}

/// Decodes one frame payload. The payload must contain exactly one message.
pub fn decode_message(payload: &[u8]) -> Result<Message, FrameError> {
    let (message, rest) = postcard::take_from_bytes(payload)?;
    if !rest.is_empty() {
        return Err(FrameError::TrailingBytes(rest.len()));
    }
    Ok(message)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hello() -> Message {
        Message::Hello(Hello {
            client_version: Text::new("0.1.0").unwrap(),
            platform: Platform {
                os: Os::MacOs,
                arch: Arch::Aarch64,
            },
        })
    }

    fn payload(frame: &[u8]) -> &[u8] {
        &frame[FRAME_HEADER_LEN..]
    }

    #[test]
    fn preamble_round_trips() {
        let bytes = encode_preamble(PROTOCOL_VERSION);
        assert_eq!(decode_preamble(bytes), Ok(PROTOCOL_VERSION));
    }

    #[test]
    fn preamble_rejects_foreign_streams() {
        let mut bytes = encode_preamble(7);
        bytes[0] = b'X';
        assert_eq!(decode_preamble(bytes), Err(PreambleError::BadMagic));
    }

    /// The preamble layout is the one thing every future release must still
    /// understand. Changing these bytes breaks version-mismatch reporting.
    #[test]
    fn preamble_layout_is_frozen() {
        assert_eq!(
            encode_preamble(0x0102_0304),
            [b'T', b'P', b'F', b'3', b'M', b'P', 0x04, 0x03, 0x02, 0x01]
        );
    }

    /// Guards against accidental wire changes such as reordered variants or
    /// fields. Update deliberately, together with `PROTOCOL_VERSION`.
    #[test]
    fn hello_wire_format_is_stable() {
        let frame = encode_frame(&hello(), CONTROL_MAX_FRAME).unwrap();
        assert_eq!(
            frame,
            [
                9, 0, 0, 0, // payload length
                0, // Message::Hello
                5, b'0', b'.', b'1', b'.', b'0', // client_version
                2,    // Os::MacOs
                1,    // Arch::Aarch64
            ]
        );
    }

    #[test]
    fn every_message_round_trips() {
        let messages = [
            hello(),
            Message::Welcome(Welcome {
                server_version: Text::new("0.1.0").unwrap(),
                session_id: SessionId([7; 16]),
            }),
            Message::Reject(Reject {
                reason: RejectReason::ServerFull,
            }),
        ];
        for message in messages {
            let frame = encode_frame(&message, CONTROL_MAX_FRAME).unwrap();
            let header = frame[..FRAME_HEADER_LEN].try_into().unwrap();
            assert_eq!(
                frame_len(header, CONTROL_MAX_FRAME).unwrap(),
                frame.len() - FRAME_HEADER_LEN
            );
            assert_eq!(decode_message(payload(&frame)).unwrap(), message);
        }
    }

    #[test]
    fn frame_len_enforces_the_cap() {
        let max = 16;
        assert!(matches!(
            frame_len(17u32.to_le_bytes(), max),
            Err(FrameError::TooLarge { len: 17, max: 16 })
        ));
        assert!(matches!(
            frame_len(u32::MAX.to_le_bytes(), max),
            Err(FrameError::TooLarge { .. })
        ));
        assert!(matches!(
            frame_len(0u32.to_le_bytes(), max),
            Err(FrameError::Empty)
        ));
        assert_eq!(frame_len(16u32.to_le_bytes(), max).unwrap(), 16);
    }

    #[test]
    fn encoder_refuses_frames_above_the_cap() {
        assert!(matches!(
            encode_frame(&hello(), 4),
            Err(FrameError::TooLarge { len: 9, max: 4 })
        ));
    }

    #[test]
    fn decoder_rejects_truncated_and_padded_payloads() {
        let frame = encode_frame(&hello(), CONTROL_MAX_FRAME).unwrap();
        let body = payload(&frame);
        assert!(matches!(
            decode_message(&body[..body.len() - 1]),
            Err(FrameError::Malformed(_))
        ));
        let mut padded = body.to_vec();
        padded.push(0);
        assert!(matches!(
            decode_message(&padded),
            Err(FrameError::TrailingBytes(1))
        ));
    }

    #[test]
    fn decoder_enforces_text_rules() {
        // A hello whose client_version carries an escape sequence.
        let mut body = vec![0, 4];
        body.extend_from_slice(b"\x1b[2J");
        body.extend_from_slice(&[0, 0]);
        assert!(matches!(
            decode_message(&body),
            Err(FrameError::Malformed(_))
        ));
    }

    #[test]
    fn decoder_rejects_unknown_variants() {
        assert!(matches!(
            decode_message(&[200, 0]),
            Err(FrameError::Malformed(_))
        ));
    }

    #[test]
    fn session_id_displays_as_hex() {
        let mut bytes = [0; 16];
        bytes[0] = 0xab;
        bytes[15] = 0x01;
        assert_eq!(
            SessionId(bytes).to_string(),
            "s-ab000000000000000000000000000001"
        );
    }
}
