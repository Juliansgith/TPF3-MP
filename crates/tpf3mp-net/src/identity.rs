//! Player identity: a per-install Ed25519 key, and proofs of it bound to one
//! TLS session (see "Handshake and identity" in `docs/PROTOCOL.md`).

use std::{
    fmt, fs, io,
    path::{Path, PathBuf},
};

use ring::{
    rand::{SecureRandom, SystemRandom},
    signature::{ED25519, Ed25519KeyPair, KeyPair, UnparsedPublicKey},
};
use thiserror::Error;
use tpf3mp_proto::{AUTH_DOMAIN, AUTH_EXPORTER_LABEL, FixedBytes, PlayerId, Signature};

#[derive(Debug, Error)]
pub enum IdentityError {
    #[error("the identity key is not a valid Ed25519 PKCS#8 key")]
    InvalidKey,
    #[error("cannot generate an identity key")]
    Generate,
    #[error("cannot access the identity file {path}: {source}")]
    Io { path: PathBuf, source: io::Error },
    #[error("the connection cannot export TLS keying material")]
    KeyingMaterial,
}

/// A player's signing key. `Debug` never shows key material.
pub struct Identity {
    key_pair: Ed25519KeyPair,
    player: PlayerId,
}

impl Identity {
    /// Generates a new identity and returns it with its PKCS#8 encoding, which
    /// is what gets stored.
    pub fn generate() -> Result<(Self, Vec<u8>), IdentityError> {
        let pkcs8 = Ed25519KeyPair::generate_pkcs8(&SystemRandom::new())
            .map_err(|_| IdentityError::Generate)?;
        let identity = Self::from_pkcs8(pkcs8.as_ref())?;
        Ok((identity, pkcs8.as_ref().to_vec()))
    }

    pub fn from_pkcs8(pkcs8: &[u8]) -> Result<Self, IdentityError> {
        let key_pair = Ed25519KeyPair::from_pkcs8(pkcs8).map_err(|_| IdentityError::InvalidKey)?;
        let public: [u8; 32] = key_pair
            .public_key()
            .as_ref()
            .try_into()
            .map_err(|_| IdentityError::InvalidKey)?;
        Ok(Self {
            key_pair,
            player: PlayerId(FixedBytes(public)),
        })
    }

    /// Loads the identity stored at `path`, creating it on first use. The
    /// file is written atomically and, on Unix, readable only by its owner.
    /// If two processes create it at once, both end up with the one that
    /// was stored.
    pub fn load_or_create(path: &Path) -> Result<Self, IdentityError> {
        let io_error = |source| IdentityError::Io {
            path: path.to_owned(),
            source,
        };
        match fs::read(path) {
            Ok(pkcs8) => Self::from_pkcs8(&pkcs8),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                let (identity, pkcs8) = Self::generate()?;
                if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
                    fs::create_dir_all(parent).map_err(io_error)?;
                }
                let mut suffix = [0; 8];
                SystemRandom::new()
                    .fill(&mut suffix)
                    .map_err(|_| IdentityError::Generate)?;
                let suffix: String = suffix.iter().map(|byte| format!("{byte:02x}")).collect();
                let temporary = path.with_extension(format!("{suffix}.tmp"));
                write_private(&temporary, &pkcs8).map_err(io_error)?;
                let published = publish(&temporary, path);
                let _ = fs::remove_file(&temporary);
                match published {
                    Ok(()) => Ok(identity),
                    // Another process stored its identity first: use that.
                    Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                        Self::from_pkcs8(&fs::read(path).map_err(io_error)?)
                    }
                    Err(error) => Err(io_error(error)),
                }
            }
            Err(error) => Err(io_error(error)),
        }
    }

    pub fn player(&self) -> PlayerId {
        self.player
    }

    /// Signs the proof of this identity for `connection`.
    pub fn prove(&self, connection: &quinn::Connection) -> Result<Signature, IdentityError> {
        let message = auth_message(connection)?;
        let signature = self.key_pair.sign(&message);
        let bytes: [u8; 64] = signature
            .as_ref()
            .try_into()
            .map_err(|_| IdentityError::InvalidKey)?;
        Ok(Signature(FixedBytes(bytes)))
    }
}

impl fmt::Debug for Identity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Identity")
            .field("player", &self.player)
            .finish_non_exhaustive()
    }
}

/// Whether `proof` proves `player` on this exact connection. Keys of small
/// order are refused: a signature can verify under them without anyone
/// holding a private key, so anyone could claim such a player.
pub fn verify_proof(connection: &quinn::Connection, player: &PlayerId, proof: &Signature) -> bool {
    if has_small_order(player.as_bytes()) {
        return false;
    }
    let Ok(message) = auth_message(connection) else {
        return false;
    };
    UnparsedPublicKey::new(&ED25519, player.as_bytes())
        .verify(&message, &proof.0.0)
        .is_ok()
}

/// Encodings of the Ed25519 points of small order, including non-canonical
/// ones, as libsodium lists them. The sign bit (the top bit of the last
/// byte) is ignored when comparing.
const SMALL_ORDER: [[u8; 32]; 7] = [
    // 0 (order 4)
    [0; 32],
    // 1 (order 1)
    [
        0x01, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
        0, 0, 0,
    ],
    // Order 8
    [
        0x26, 0xe8, 0x95, 0x8f, 0xc2, 0xb2, 0x27, 0xb0, 0x45, 0xc3, 0xf4, 0x89, 0xf2, 0xef, 0x98,
        0xf0, 0xd5, 0xdf, 0xac, 0x05, 0xd3, 0xc6, 0x33, 0x39, 0xb1, 0x38, 0x02, 0x88, 0x6d, 0x53,
        0xfc, 0x05,
    ],
    // Order 8
    [
        0xc7, 0x17, 0x6a, 0x70, 0x3d, 0x4d, 0xd8, 0x4f, 0xba, 0x3c, 0x0b, 0x76, 0x0d, 0x10, 0x67,
        0x0f, 0x2a, 0x20, 0x53, 0xfa, 0x2c, 0x39, 0xcc, 0xc6, 0x4e, 0xc7, 0xfd, 0x77, 0x92, 0xac,
        0x03, 0x7a,
    ],
    // p - 1 (order 2)
    [
        0xec, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff,
        0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff,
        0xff, 0x7f,
    ],
    // p, a non-canonical 0 (order 4)
    [
        0xed, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff,
        0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff,
        0xff, 0x7f,
    ],
    // p + 1, a non-canonical 1 (order 1)
    [
        0xee, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff,
        0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff,
        0xff, 0x7f,
    ],
];

fn has_small_order(key: &[u8]) -> bool {
    let Ok(key) = <&[u8; 32]>::try_from(key) else {
        return false;
    };
    SMALL_ORDER
        .iter()
        .any(|point| key[..31] == point[..31] && key[31] & 0x7f == point[31])
}

/// `AUTH_DOMAIN || E`, where `E` is keying material both TLS endpoints derive
/// from this session's secrets and no third party can compute.
fn auth_message(connection: &quinn::Connection) -> Result<Vec<u8>, IdentityError> {
    let mut keying_material = [0; 32];
    connection
        .export_keying_material(&mut keying_material, AUTH_EXPORTER_LABEL, b"")
        .map_err(|_| IdentityError::KeyingMaterial)?;
    let mut message = Vec::with_capacity(AUTH_DOMAIN.len() + keying_material.len());
    message.extend_from_slice(AUTH_DOMAIN);
    message.extend_from_slice(&keying_material);
    Ok(message)
}

/// Makes the complete file at `temporary` appear at `path`, failing with
/// `AlreadyExists` rather than replacing a file there. A hard link does
/// that atomically; file systems without hard links fall back to a rename.
fn publish(temporary: &Path, path: &Path) -> io::Result<()> {
    match fs::hard_link(temporary, path) {
        Err(error) if error.kind() != io::ErrorKind::AlreadyExists => {
            if path.exists() {
                return Err(io::ErrorKind::AlreadyExists.into());
            }
            fs::rename(temporary, path)
        }
        result => result,
    }
}

fn write_private(path: &Path, bytes: &[u8]) -> io::Result<()> {
    use std::io::Write;
    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(path)?;
    file.write_all(bytes)?;
    file.sync_all()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identity_survives_a_round_trip_through_pkcs8() {
        let (identity, pkcs8) = Identity::generate().unwrap();
        let reloaded = Identity::from_pkcs8(&pkcs8).unwrap();
        assert_eq!(identity.player(), reloaded.player());
    }

    #[test]
    fn garbage_is_not_an_identity() {
        assert!(matches!(
            Identity::from_pkcs8(&[0; 48]),
            Err(IdentityError::InvalidKey)
        ));
    }

    #[test]
    fn load_or_create_is_stable() {
        let dir = std::env::temp_dir().join(format!("tpf3mp-identity-{}", std::process::id()));
        let path = dir.join("nested").join("identity.key");
        let first = Identity::load_or_create(&path).unwrap();
        let second = Identity::load_or_create(&path).unwrap();
        assert_eq!(first.player(), second.player());
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn processes_creating_an_identity_at_once_agree_on_it() {
        let dir = std::env::temp_dir().join(format!("tpf3mp-race-{}", std::process::id()));
        let path = dir.join("identity.key");
        let players: Vec<PlayerId> = std::thread::scope(|scope| {
            let racers: Vec<_> = (0..8)
                .map(|_| scope.spawn(|| Identity::load_or_create(&path).unwrap().player()))
                .collect();
            racers
                .into_iter()
                .map(|racer| racer.join().unwrap())
                .collect()
        });
        let stored = Identity::load_or_create(&path).unwrap().player();
        assert!(
            players.iter().all(|player| *player == stored),
            "{players:?}"
        );
        // Only the key is left behind.
        assert_eq!(fs::read_dir(&dir).unwrap().count(), 1);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn keys_of_small_order_are_recognized_with_either_sign() {
        for point in SMALL_ORDER {
            let mut flipped = point;
            flipped[31] |= 0x80;
            assert!(has_small_order(&point), "{point:02x?}");
            assert!(has_small_order(&flipped), "{flipped:02x?}");
        }
        for _ in 0..100 {
            let (identity, _) = Identity::generate().unwrap();
            assert!(!has_small_order(identity.player().as_bytes()));
        }
    }

    #[test]
    fn debug_hides_key_material() {
        let (identity, _) = Identity::generate().unwrap();
        let debug = format!("{identity:?}");
        assert!(debug.starts_with("Identity { player: p-"), "{debug}");
        assert!(debug.ends_with(".. }"), "{debug}");
    }
}
