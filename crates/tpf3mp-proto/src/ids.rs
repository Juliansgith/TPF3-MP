use std::{fmt, str::FromStr};

use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::bytes::{FixedBytes, write_hex};

/// Non-secret identifier of one connection, safe to quote in bug reports.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct SessionId(pub [u8; 16]);

impl fmt::Display for SessionId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("s-")?;
        write_hex(f, &self.0)
    }
}

impl fmt::Debug for SessionId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(self, f)
    }
}

/// A player's identity: their per-install Ed25519 public key.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct PlayerId(pub FixedBytes<32>);

impl PlayerId {
    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0.0
    }
}

impl fmt::Display for PlayerId {
    /// A short fingerprint for logs and UI; the full key is in `Debug`.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("p-")?;
        write_hex(f, &self.0.0[..8])
    }
}

impl fmt::Debug for PlayerId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("p-")?;
        write_hex(f, &self.0.0)
    }
}

/// An Ed25519 signature.
#[derive(Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Signature(pub FixedBytes<64>);

impl fmt::Debug for Signature {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Signature(..)")
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct RoomId(pub FixedBytes<16>);

impl fmt::Display for RoomId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("r-")?;
        write_hex(f, &self.0.0)
    }
}

impl fmt::Debug for RoomId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(self, f)
    }
}

/// Everything needed to join a room: its ID and a secret 256-bit token.
///
/// The text form is `TPF3MP1.` followed by base64url of the 16-byte room ID
/// and the 32-byte token. `Debug` never shows the token, so invites cannot
/// leak into logs through formatting.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Invite {
    pub room: RoomId,
    pub token: FixedBytes<32>,
}

const INVITE_PREFIX: &str = "TPF3MP1.";
const INVITE_BYTES: usize = 16 + 32;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum InviteError {
    #[error("this is not a TPF3-MP invite")]
    Prefix,
    #[error("the invite is damaged; copy it again")]
    Malformed,
}

impl fmt::Display for Invite {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut bytes = [0; INVITE_BYTES];
        bytes[..16].copy_from_slice(&self.room.0.0);
        bytes[16..].copy_from_slice(&self.token.0);
        write!(f, "{INVITE_PREFIX}{}", URL_SAFE_NO_PAD.encode(bytes))
    }
}

impl fmt::Debug for Invite {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Invite")
            .field("room", &self.room)
            .finish_non_exhaustive()
    }
}

impl FromStr for Invite {
    type Err = InviteError;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        let encoded = text
            .trim()
            .strip_prefix(INVITE_PREFIX)
            .ok_or(InviteError::Prefix)?;
        let decoded = URL_SAFE_NO_PAD
            .decode(encoded)
            .map_err(|_| InviteError::Malformed)?;
        let bytes: [u8; INVITE_BYTES] = decoded.try_into().map_err(|_| InviteError::Malformed)?;
        let mut room = [0; 16];
        let mut token = [0; 32];
        room.copy_from_slice(&bytes[..16]);
        token.copy_from_slice(&bytes[16..]);
        Ok(Self {
            room: RoomId(FixedBytes(room)),
            token: FixedBytes(token),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn invite() -> Invite {
        Invite {
            room: RoomId(FixedBytes([0xab; 16])),
            token: FixedBytes([0x5c; 32]),
        }
    }

    #[test]
    fn invite_text_round_trips() {
        let text = invite().to_string();
        assert!(text.starts_with("TPF3MP1."));
        assert_eq!(text.len(), 8 + 64);
        assert_eq!(text.parse::<Invite>().unwrap(), invite());
        // Pasting often adds whitespace.
        assert_eq!(format!("  {text}\n").parse::<Invite>().unwrap(), invite());
    }

    #[test]
    fn invite_rejects_foreign_and_damaged_text() {
        assert_eq!("TPF2MP1.abc".parse::<Invite>(), Err(InviteError::Prefix));
        let text = invite().to_string();
        assert_eq!(
            text[..text.len() - 1].parse::<Invite>(),
            Err(InviteError::Malformed)
        );
        assert_eq!(
            "TPF3MP1.!!!!".parse::<Invite>(),
            Err(InviteError::Malformed)
        );
    }

    #[test]
    fn invite_debug_hides_the_token() {
        let debug = format!("{:?}", invite());
        assert!(debug.contains("r-abab"), "{debug}");
        assert!(!debug.to_lowercase().contains("5c5c"), "{debug}");
    }

    #[test]
    fn player_display_is_a_short_fingerprint() {
        let mut key = [0; 32];
        key[0] = 0x12;
        let player = PlayerId(FixedBytes(key));
        assert_eq!(player.to_string(), "p-1200000000000000");
        assert_eq!(format!("{player:?}").len(), 2 + 64);
    }
}
