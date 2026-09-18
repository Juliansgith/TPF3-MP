//! Player identity: a per-install Ed25519 key, and proofs of it bound to one
//! TLS session (see "Handshake and identity" in `docs/PROTOCOL.md`).

use std::{
    fmt, fs, io,
    path::{Path, PathBuf},
};

use ring::{
    rand::SystemRandom,
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
                let temporary = path.with_extension("tmp");
                write_private(&temporary, &pkcs8).map_err(io_error)?;
                fs::rename(&temporary, path).map_err(io_error)?;
                Ok(identity)
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

/// Whether `proof` proves `player` on this exact connection.
pub fn verify_proof(connection: &quinn::Connection, player: &PlayerId, proof: &Signature) -> bool {
    let Ok(message) = auth_message(connection) else {
        return false;
    };
    UnparsedPublicKey::new(&ED25519, player.as_bytes())
        .verify(&message, &proof.0.0)
        .is_ok()
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

fn write_private(path: &Path, bytes: &[u8]) -> io::Result<()> {
    use std::io::Write;
    let mut options = fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
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
    fn debug_hides_key_material() {
        let (identity, _) = Identity::generate().unwrap();
        let debug = format!("{identity:?}");
        assert!(debug.starts_with("Identity { player: p-"), "{debug}");
        assert!(debug.ends_with(".. }"), "{debug}");
    }
}
