use std::{fmt, path::Path, sync::Arc, time::Duration};

use quinn::crypto::rustls::{NoInitialCipherSuite, QuicClientConfig, QuicServerConfig};
use rustls::pki_types::{
    CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer,
    pem::{self, PemObject},
};
use thiserror::Error;
use tpf3mp_proto::ALPN;

/// A connection with no traffic, not even keep-alives, is dropped after this.
const IDLE_TIMEOUT: Duration = Duration::from_secs(30);
/// Clients send keep-alives to hold their NAT binding open. The server sends
/// none, so a client that goes silent times out.
const KEEP_ALIVE_INTERVAL: Duration = Duration::from_secs(5);
/// How much unread data a client may send the server on its stream, and on
/// the whole connection. The server reads its control stream as frames
/// arrive, so these bound what a connection can make it hold without
/// slowing honest clients.
const SERVER_STREAM_WINDOW: u32 = 256 * 1024;
const SERVER_CONNECTION_WINDOW: u32 = 512 * 1024;
/// Turn streams the server may have open to a client at once. There is one
/// per game the client enters; the rest leave room for one that is ending.
const TURN_STREAMS: u32 = 4;

#[derive(Debug, Error)]
pub enum TlsError {
    #[error("cannot read PEM data: {0}")]
    Pem(#[from] pem::Error),
    #[error("no certificate found in the certificate file")]
    NoCertificate,
    #[error("TLS configuration rejected: {0}")]
    Rustls(#[from] rustls::Error),
    #[error("certificate generation failed: {0}")]
    Generate(#[from] rcgen::Error),
    #[error("the TLS configuration offers no cipher suite QUIC can use")]
    NoInitialCipherSuite(#[from] NoInitialCipherSuite),
}

/// The server's certificate chain and private key.
pub struct ServerIdentity {
    chain: Vec<CertificateDer<'static>>,
    key: PrivateKeyDer<'static>,
}

impl ServerIdentity {
    /// Loads a PEM certificate chain (leaf first) and its PEM private key,
    /// such as the files an ACME client writes.
    pub fn from_pem_files(certificate: &Path, key: &Path) -> Result<Self, TlsError> {
        let chain = CertificateDer::pem_file_iter(certificate)?.collect::<Result<Vec<_>, _>>()?;
        if chain.is_empty() {
            return Err(TlsError::NoCertificate);
        }
        let key = PrivateKeyDer::from_pem_file(key)?;
        Ok(Self { chain, key })
    }

    /// Generates a self-signed certificate for development servers. Clients
    /// must pin it with [`ServerTrust::Pinned`].
    pub fn self_signed(names: &[&str]) -> Result<Self, TlsError> {
        let names: Vec<String> = names.iter().map(|name| (*name).to_owned()).collect();
        let generated = rcgen::generate_simple_self_signed(names)?;
        let key = PrivatePkcs8KeyDer::from(generated.signing_key.serialize_der());
        Ok(Self {
            chain: vec![generated.cert.der().clone()],
            key: key.into(),
        })
    }

    /// The leaf certificate, for pinning.
    pub fn leaf(&self) -> &CertificateDer<'static> {
        // Both constructors guarantee a non-empty chain.
        &self.chain[0]
    }
}

impl fmt::Debug for ServerIdentity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Never print the private key.
        f.debug_struct("ServerIdentity")
            .field("certificates", &self.chain.len())
            .finish_non_exhaustive()
    }
}

/// How a client decides whether to trust the server's certificate.
#[derive(Debug, Clone)]
pub enum ServerTrust {
    /// Public certificate authorities (Mozilla's root set). For deployed
    /// servers with a real certificate, e.g. from Let's Encrypt.
    WebPki,
    /// Exactly this certificate. For development servers.
    Pinned(CertificateDer<'static>),
}

pub fn server_config(identity: ServerIdentity) -> Result<quinn::ServerConfig, TlsError> {
    let mut tls = rustls::ServerConfig::builder_with_provider(crypto_provider())
        .with_protocol_versions(&[&rustls::version::TLS13])?
        .with_no_client_auth()
        .with_single_cert(identity.chain, identity.key)?;
    tls.alpn_protocols = vec![ALPN.to_vec()];
    let quic = QuicServerConfig::try_from(tls)?;
    let mut config = quinn::ServerConfig::with_crypto(Arc::new(quic));
    config.transport_config(Arc::new(server_transport()));
    Ok(config)
}

pub fn client_config(trust: ServerTrust) -> Result<quinn::ClientConfig, TlsError> {
    let mut roots = rustls::RootCertStore::empty();
    match trust {
        ServerTrust::WebPki => roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned()),
        ServerTrust::Pinned(certificate) => roots.add(certificate)?,
    }
    let mut tls = rustls::ClientConfig::builder_with_provider(crypto_provider())
        .with_protocol_versions(&[&rustls::version::TLS13])?
        .with_root_certificates(roots)
        .with_no_client_auth();
    tls.alpn_protocols = vec![ALPN.to_vec()];
    let quic = QuicClientConfig::try_from(tls)?;
    let mut config = quinn::ClientConfig::new(Arc::new(quic));
    config.transport_config(Arc::new(client_transport()));
    Ok(config)
}

fn crypto_provider() -> Arc<rustls::crypto::CryptoProvider> {
    Arc::new(rustls::crypto::ring::default_provider())
}

/// What a client may make the server hold. A client opens exactly one
/// stream, the control stream, and sends no datagrams; anything else would
/// only sit in the server's buffers unread.
fn server_transport() -> quinn::TransportConfig {
    let mut transport = base_transport();
    transport
        .max_concurrent_bidi_streams(1u32.into())
        .max_concurrent_uni_streams(0u32.into())
        .stream_receive_window(SERVER_STREAM_WINDOW.into())
        .receive_window(SERVER_CONNECTION_WINDOW.into())
        .datagram_receive_buffer_size(None);
    transport
}

/// The server opens turn streams and nothing else.
fn client_transport() -> quinn::TransportConfig {
    let mut transport = base_transport();
    transport
        .keep_alive_interval(Some(KEEP_ALIVE_INTERVAL))
        .max_concurrent_bidi_streams(0u32.into())
        .max_concurrent_uni_streams(TURN_STREAMS.into())
        .datagram_receive_buffer_size(None);
    transport
}

fn base_transport() -> quinn::TransportConfig {
    let mut transport = quinn::TransportConfig::default();
    transport.max_idle_timeout(Some(
        quinn::IdleTimeout::try_from(IDLE_TIMEOUT).expect("30 s is a valid QUIC idle timeout"),
    ));
    transport
}
