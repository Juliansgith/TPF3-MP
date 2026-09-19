//! QUIC endpoints, TLS configuration, player identity, framed stream I/O
//! and QUIC over WebSocket, shared by the server and the agent.

pub mod bulk;
pub mod close;
mod identity;
mod io;
mod tls;
pub mod tunnel;

pub use identity::{Identity, IdentityError, verify_proof};
pub use io::{NetError, read_message, read_preamble, write_frame, write_message, write_preamble};
pub use rustls::pki_types::CertificateDer;
pub use tls::{
    ServerIdentity, ServerTrust, TlsError, client_config, server_config, tunnel_server_tls,
};
