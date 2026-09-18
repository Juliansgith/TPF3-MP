//! QUIC application close codes.
//!
//! These values are part of the protocol and never change meaning: an old
//! client must still be able to explain why a newer server closed it.

use quinn::VarInt;

/// Orderly end of a session.
pub const NORMAL: VarInt = VarInt::from_u32(0);
/// The peer sent something the protocol does not allow.
pub const PROTOCOL_VIOLATION: VarInt = VarInt::from_u32(1);
/// The peers speak different protocol versions.
pub const VERSION_MISMATCH: VarInt = VarInt::from_u32(2);
/// The server declined the session; the reason was sent as a `Reject` message.
pub const REJECTED: VarInt = VarInt::from_u32(3);
/// The client did not complete the handshake in time.
pub const HANDSHAKE_TIMEOUT: VarInt = VarInt::from_u32(4);
/// The server is shutting down.
pub const SHUTTING_DOWN: VarInt = VarInt::from_u32(5);
/// The client did not read what the server sent fast enough, and the
/// server's bounded buffer for it filled up.
pub const SLOW_CONSUMER: VarInt = VarInt::from_u32(6);
/// The same player connected again; this older connection was replaced.
pub const REPLACED: VarInt = VarInt::from_u32(7);
