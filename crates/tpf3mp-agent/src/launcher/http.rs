//! The page's HTTP server: plain HTTP/1.1 on the loopback interface, one
//! small request per connection.
//!
//! Browsers let any web page send requests to loopback addresses, so the
//! server trusts nothing about where a request came from:
//!
//! - Every API request must carry the launcher's secret token, which only
//!   the page opened from the launcher's own URL knows (it travels in the
//!   URL's fragment, which browsers never send anywhere).
//! - The `Host` header must name the loopback address and port, which
//!   defeats DNS rebinding.
//! - The page may not be framed, and its content security policy lets it
//!   talk to this server only.

use std::{io, sync::Arc, time::Duration};

use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    sync::{Semaphore, oneshot},
};
use tracing::warn;

use super::{Shared, api};

/// Largest request head read.
const MAX_HEAD: usize = 8 * 1024;
/// Largest request body read: an action is a few hundred bytes.
const MAX_BODY: usize = 4 * 1024;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
/// Requests answered at once; the page makes one or two.
const MAX_CONNECTIONS: usize = 16;
/// Pause after a failed accept, so the loop neither spins nor gives up.
const ACCEPT_BACKOFF: Duration = Duration::from_millis(100);
/// The header that carries the token.
const TOKEN_HEADER: &str = "x-launcher-token";

const PAGE: &str = include_str!("page.html");

const SECURITY_HEADERS: &str = "Cache-Control: no-store\r\n\
    X-Frame-Options: DENY\r\n\
    X-Content-Type-Options: nosniff\r\n\
    Referrer-Policy: no-referrer\r\n\
    Content-Security-Policy: default-src 'none'; script-src 'unsafe-inline'; \
    style-src 'unsafe-inline'; connect-src 'self'; img-src data:; \
    frame-ancestors 'none'; base-uri 'none'; form-action 'none'\r\n";

/// Serves the page and its API for as long as the launcher runs.
pub(crate) async fn serve(listener: TcpListener, shared: Arc<Shared>) {
    let slots = Arc::new(Semaphore::new(MAX_CONNECTIONS));
    loop {
        let Ok(slot) = Arc::clone(&slots).acquire_owned().await else {
            return;
        };
        let stream = match listener.accept().await {
            Ok((stream, _)) => stream,
            Err(error) => {
                warn!(%error, "the launcher cannot accept a connection");
                tokio::time::sleep(ACCEPT_BACKOFF).await;
                continue;
            }
        };
        let shared = Arc::clone(&shared);
        tokio::spawn(async move {
            let _ = tokio::time::timeout(REQUEST_TIMEOUT, answer(stream, &shared)).await;
            drop(slot);
        });
    }
}

/// One parsed request.
#[derive(Debug)]
pub(crate) struct Request {
    pub(crate) method: String,
    pub(crate) path: String,
    pub(crate) headers: Vec<(String, String)>,
    pub(crate) body: Vec<u8>,
}

impl Request {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }
}

/// What the server answers.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Response {
    pub(crate) status: &'static str,
    pub(crate) content_type: &'static str,
    pub(crate) body: String,
}

impl Response {
    fn json(status: &'static str, body: String) -> Self {
        Self {
            status,
            content_type: "application/json",
            body,
        }
    }

    fn error(status: &'static str, message: &str) -> Self {
        Self::json(status, serde_json::json!({ "error": message }).to_string())
    }
}

async fn answer(mut stream: TcpStream, shared: &Shared) -> io::Result<()> {
    let Some(request) = read_request(&mut stream).await? else {
        return Ok(());
    };
    let response = respond(&request, shared).await;
    let head = format!(
        "HTTP/1.1 {}\r\nContent-Type: {}; charset=utf-8\r\nContent-Length: {}\r\n{SECURITY_HEADERS}Connection: close\r\n\r\n",
        response.status,
        response.content_type,
        response.body.len()
    );
    stream.write_all(head.as_bytes()).await?;
    stream.write_all(response.body.as_bytes()).await?;
    stream.shutdown().await
}

/// Reads one request: a bounded head, then a bounded body. `None` for
/// anything that is not a well-formed, small request.
async fn read_request(stream: &mut TcpStream) -> io::Result<Option<Request>> {
    let mut buffer = Vec::with_capacity(1024);
    let mut chunk = [0; 1024];
    let head_end = loop {
        if let Some(end) = buffer.windows(4).position(|window| window == b"\r\n\r\n") {
            break end;
        }
        let read = stream.read(&mut chunk).await?;
        if read == 0 || buffer.len() + read > MAX_HEAD + MAX_BODY {
            return Ok(None);
        }
        buffer.extend_from_slice(&chunk[..read]);
        if buffer.len() > MAX_HEAD && !buffer.windows(4).any(|window| window == b"\r\n\r\n") {
            return Ok(None);
        }
    };
    let Ok(head) = std::str::from_utf8(&buffer[..head_end]) else {
        return Ok(None);
    };
    let mut lines = head.split("\r\n");
    let mut request_line = lines.next().unwrap_or_default().split(' ');
    let (Some(method), Some(path), Some(version)) = (
        request_line.next(),
        request_line.next(),
        request_line.next(),
    ) else {
        return Ok(None);
    };
    if !version.starts_with("HTTP/1.") {
        return Ok(None);
    }
    let mut headers = Vec::new();
    for line in lines {
        let Some((key, value)) = line.split_once(':') else {
            return Ok(None);
        };
        headers.push((key.trim().to_owned(), value.trim().to_owned()));
    }
    let mut request = Request {
        method: method.to_owned(),
        path: path.to_owned(),
        headers,
        body: Vec::new(),
    };
    let length = match request.header("content-length") {
        Some(length) => match length.parse::<usize>() {
            Ok(length) if length <= MAX_BODY => length,
            _ => return Ok(None),
        },
        None => 0,
    };
    let mut body = buffer[head_end + 4..].to_vec();
    while body.len() < length {
        let read = stream.read(&mut chunk).await?;
        if read == 0 {
            return Ok(None);
        }
        body.extend_from_slice(&chunk[..read]);
    }
    body.truncate(length);
    request.body = body;
    Ok(Some(request))
}

/// Answers a request.
pub(crate) async fn respond(request: &Request, shared: &Shared) -> Response {
    let port = shared.address.port();
    let host_ok = request.header("host").is_some_and(|host| {
        [
            format!("127.0.0.1:{port}"),
            format!("localhost:{port}"),
            format!("[::1]:{port}"),
        ]
        .iter()
        .any(|allowed| allowed.eq_ignore_ascii_case(host))
    });
    if !host_ok {
        return Response::error("403 Forbidden", "wrong host");
    }
    let path = request.path.split('?').next().unwrap_or_default();
    match (request.method.as_str(), path) {
        ("GET", "/") => Response {
            status: "200 OK",
            content_type: "text/html",
            body: PAGE.to_owned(),
        },
        ("GET", "/api/state") => {
            if !authorized(request, shared) {
                return Response::error("401 Unauthorized", "missing or wrong token");
            }
            let body = api::render(&shared.view(), &shared.status());
            Response::json("200 OK", body)
        }
        ("POST", "/api/action") => {
            if !authorized(request, shared) {
                return Response::error("401 Unauthorized", "missing or wrong token");
            }
            let Ok(action) = serde_json::from_slice::<api::Action>(&request.body) else {
                return Response::error("400 Bad Request", "not an action");
            };
            let (reply, answer) = oneshot::channel();
            if shared.actions.send((action, reply)).await.is_err() {
                return Response::error("503 Service Unavailable", "the launcher is stopping");
            }
            match answer.await {
                Ok(Ok(())) => Response::json("200 OK", "{\"ok\":true}".to_owned()),
                Ok(Err(error)) => Response::error("409 Conflict", &error),
                Err(_) => Response::error("503 Service Unavailable", "the launcher is stopping"),
            }
        }
        _ => Response::error("404 Not Found", "not found"),
    }
}

/// Whether the request carries the token, compared in constant time.
fn authorized(request: &Request, shared: &Shared) -> bool {
    let Some(token) = request.header(TOKEN_HEADER) else {
        return false;
    };
    let (given, expected) = (token.as_bytes(), shared.token.as_bytes());
    given.len() == expected.len()
        && given
            .iter()
            .zip(expected)
            .fold(0u8, |difference, (a, b)| difference | (a ^ b))
            == 0
}
