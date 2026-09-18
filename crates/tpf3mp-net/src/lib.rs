//! QUIC endpoints, TLS configuration and framed stream I/O shared by the
//! server and the agent.

pub mod close;
mod io;
mod tls;

pub use io::{NetError, read_message, read_preamble, write_message, write_preamble};
pub use rustls::pki_types::CertificateDer;
pub use tls::{ServerIdentity, ServerTrust, TlsError, client_config, server_config};
