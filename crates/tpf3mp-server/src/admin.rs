//! The admin endpoint: Prometheus metrics at `/metrics` and a health check at
//! `/healthz`, over plain HTTP. It has no authentication, so it must only
//! listen on a private address: loopback, or a VPN interface.

use std::{sync::Arc, time::Duration};

use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    sync::Semaphore,
};
use tracing::warn;

use crate::ServerStats;

/// Largest request head the endpoint reads.
const MAX_REQUEST: usize = 8 * 1024;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(5);
/// Requests answered at once; more wait in the listen backlog. A scraper
/// needs one.
const MAX_CONNECTIONS: usize = 16;
/// Pause after a failed accept, such as when the process is out of file
/// descriptors, so the loop neither spins nor gives up.
const ACCEPT_BACKOFF: Duration = Duration::from_millis(100);

/// Serves the admin endpoint for as long as the server runs. A failed
/// accept is logged and retried: the endpoint must not vanish silently.
pub async fn serve_admin(listener: TcpListener, stats: ServerStats) {
    let slots = Arc::new(Semaphore::new(MAX_CONNECTIONS));
    loop {
        let Ok(slot) = Arc::clone(&slots).acquire_owned().await else {
            return;
        };
        let stream = match listener.accept().await {
            Ok((stream, _)) => stream,
            Err(error) => {
                warn!(%error, "the admin endpoint cannot accept a connection");
                tokio::time::sleep(ACCEPT_BACKOFF).await;
                continue;
            }
        };
        let stats = stats.clone();
        tokio::spawn(async move {
            let _ = tokio::time::timeout(REQUEST_TIMEOUT, answer(stream, &stats)).await;
            drop(slot);
        });
    }
}

async fn answer(mut stream: TcpStream, stats: &ServerStats) -> std::io::Result<()> {
    let mut head = Vec::with_capacity(512);
    let mut chunk = [0; 512];
    while !head.windows(4).any(|window| window == b"\r\n\r\n") {
        let read = stream.read(&mut chunk).await?;
        if read == 0 || head.len() + read > MAX_REQUEST {
            return Ok(());
        }
        head.extend_from_slice(&chunk[..read]);
    }
    let request_line = head.split(|byte| *byte == b'\r').next().unwrap_or_default();
    let (status, content_type, body) = match request_line {
        b"GET /metrics HTTP/1.1" | b"GET /metrics HTTP/1.0" => (
            "200 OK",
            "text/plain; version=0.0.4",
            stats.render_metrics(),
        ),
        b"GET /healthz HTTP/1.1" | b"GET /healthz HTTP/1.0" => {
            ("200 OK", "text/plain", "ok\n".to_owned())
        }
        _ => ("404 Not Found", "text/plain", "not found\n".to_owned()),
    };
    let response = format!(
        "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(response.as_bytes()).await?;
    stream.shutdown().await
}
