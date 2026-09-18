//! The TPF3-MP client daemon that runs next to the game.
//!
//! Milestone M0 implements connecting to a server and completing the
//! handshake. The session, turn buffer, snapshot cache and the shared-memory
//! link to the in-game hook follow in M1 (see `docs/ARCHITECTURE.md`).

use std::{
    fmt, io,
    net::{Ipv4Addr, Ipv6Addr, SocketAddr},
    time::Duration,
};

use quinn::{RecvStream, SendStream};
use thiserror::Error;
use tpf3mp_net::{
    NetError, ServerTrust, TlsError, client_config, close, read_message, read_preamble,
    write_message, write_preamble,
};
use tpf3mp_proto::{
    CONTROL_MAX_FRAME, Hello, Message, PROTOCOL_VERSION, Platform, RejectReason, Text, Welcome,
};

pub struct ConnectOptions {
    pub server: SocketAddr,
    /// The name the server's certificate must be valid for.
    pub server_name: String,
    pub trust: ServerTrust,
    pub client_version: Text<64>,
    /// The protocol version announced in the preamble. Only tests change it.
    pub protocol_version: u32,
}

impl ConnectOptions {
    pub fn new(server: SocketAddr, server_name: impl Into<String>, trust: ServerTrust) -> Self {
        Self {
            server,
            server_name: server_name.into(),
            trust,
            client_version: Text::new(env!("CARGO_PKG_VERSION"))
                .expect("the crate version is short printable text"),
            protocol_version: PROTOCOL_VERSION,
        }
    }
}

#[derive(Debug, Error)]
pub enum ConnectError {
    #[error(transparent)]
    Tls(#[from] TlsError),
    #[error("cannot open a UDP socket: {0}")]
    Socket(#[from] io::Error),
    #[error("cannot start the connection: {0}")]
    Start(#[from] quinn::ConnectError),
    #[error("connection failed: {0}")]
    Connection(#[from] quinn::ConnectionError),
    #[error("{}", version_mismatch(.client, .server))]
    VersionMismatch { client: u32, server: u32 },
    #[error("the server declined: {0}")]
    Rejected(RejectReason),
    #[error("the server broke the protocol: {0}")]
    Protocol(#[from] NetError),
    #[error("the server answered the handshake with an unexpected message")]
    UnexpectedMessage,
}

fn version_mismatch(client: &u32, server: &u32) -> String {
    let advice = if server > client {
        "update TPF3-MP"
    } else {
        "the server has not been updated yet"
    };
    format!("this client speaks protocol {client} but the server speaks {server}: {advice}")
}

/// An open session with a server.
pub struct Session {
    endpoint: quinn::Endpoint,
    connection: quinn::Connection,
    welcome: Welcome,
    _control: (SendStream, RecvStream),
}

impl Session {
    pub fn welcome(&self) -> &Welcome {
        &self.welcome
    }

    pub fn rtt(&self) -> Duration {
        self.connection.rtt()
    }

    /// Completes when the connection ends, with the reason.
    pub async fn closed(&self) -> quinn::ConnectionError {
        self.connection.closed().await
    }

    /// Ends the session and waits until the server has been told.
    pub async fn close(self) {
        self.connection.close(close::NORMAL, b"client leaving");
        self.endpoint.wait_idle().await;
    }
}

impl fmt::Debug for Session {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Session")
            .field("session_id", &self.welcome.session_id)
            .finish_non_exhaustive()
    }
}

/// Connects to a server and completes the handshake.
pub async fn connect(options: ConnectOptions) -> Result<Session, ConnectError> {
    let local: SocketAddr = if options.server.is_ipv6() {
        (Ipv6Addr::UNSPECIFIED, 0).into()
    } else {
        (Ipv4Addr::UNSPECIFIED, 0).into()
    };
    let mut endpoint = quinn::Endpoint::client(local)?;
    endpoint.set_default_client_config(client_config(options.trust.clone())?);
    let connection = endpoint
        .connect(options.server, &options.server_name)?
        .await?;
    match handshake(&connection, &options).await {
        Ok((welcome, control)) => Ok(Session {
            endpoint,
            connection,
            welcome,
            _control: control,
        }),
        Err(error) => {
            let code = match &error {
                ConnectError::VersionMismatch { .. } => close::VERSION_MISMATCH,
                ConnectError::Rejected(_) => close::NORMAL,
                _ => close::PROTOCOL_VIOLATION,
            };
            connection.close(code, b"");
            endpoint.wait_idle().await;
            Err(error)
        }
    }
}

async fn handshake(
    connection: &quinn::Connection,
    options: &ConnectOptions,
) -> Result<(Welcome, (SendStream, RecvStream)), ConnectError> {
    let failed = |error| stream_failure(connection, error);
    let (mut send, mut recv) = connection.open_bi().await?;
    write_preamble(&mut send, options.protocol_version)
        .await
        .map_err(failed)?;
    let server_protocol = read_preamble(&mut recv).await.map_err(failed)?;
    if server_protocol != options.protocol_version {
        return Err(ConnectError::VersionMismatch {
            client: options.protocol_version,
            server: server_protocol,
        });
    }
    let hello = Message::Hello(Hello {
        client_version: options.client_version.clone(),
        platform: Platform::current(),
    });
    write_message(&mut send, &hello, CONTROL_MAX_FRAME)
        .await
        .map_err(failed)?;
    match read_message(&mut recv, CONTROL_MAX_FRAME)
        .await
        .map_err(failed)?
    {
        Message::Welcome(welcome) => Ok((welcome, (send, recv))),
        Message::Reject(reject) => Err(ConnectError::Rejected(reject.reason)),
        Message::Hello(_) => Err(ConnectError::UnexpectedMessage),
    }
}

/// A stream error caused by the server closing the connection is reported as
/// the server's close reason, which says why.
fn stream_failure(connection: &quinn::Connection, error: NetError) -> ConnectError {
    match connection.close_reason() {
        Some(reason) => ConnectError::Connection(reason),
        None => ConnectError::Protocol(error),
    }
}
