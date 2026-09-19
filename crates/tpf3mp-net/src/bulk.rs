//! Bulk streams: moving a world snapshot between two chunk stores over one
//! QUIC stream. See "Snapshots" in `docs/PROTOCOL.md`.
//!
//! The side that holds a snapshot [serves](serve) it; the other side
//! [fetches](fetch) it. Fetching asks for the manifest, checks it against the
//! snapshot it was offered, then asks for every chunk its store lacks, with a
//! bounded amount outstanding so the pipe stays full. Every chunk is checked
//! as it arrives (`tpf3mp_snapshot::ChunkSink`), so a hostile peer can waste
//! bandwidth but never place a wrong byte. Serving answers only for the one
//! manifest it was given, so a peer cannot probe a store for other content.
//!
//! Which side opened the stream and why (`BulkOpen`) is the caller's
//! business; these functions run once both sides know what moves.
//!
//! Store calls do blocking file I/O and run on Tokio's blocking pool.

use std::{
    collections::{HashSet, VecDeque},
    path::PathBuf,
    time::Duration,
};

use quinn::{RecvStream, SendStream};
use thiserror::Error;
use tpf3mp_proto::{
    BULK_REQUEST_MAX_FRAME, BULK_RESPONSE_MAX_FRAME, BulkRequest, BulkResponse, ChunkHash,
    FixedBytes, MAX_CHUNKS_PER_REQUEST, SnapshotId,
};
use tpf3mp_snapshot::{
    ChunkId, ChunkSink, ChunkStore, MAX_MANIFEST_LEN, Manifest, ManifestError, ManifestId,
    Progress, SinkError, StoreError,
};

use crate::{NetError, read_message, write_message};

/// Uncompressed chunk bytes a fetch keeps requested but unanswered: enough to
/// cover the bandwidth-delay product of a fast home connection (1 Gbit/s at
/// 100 ms is 12.5 MB) without asking for more than a peer should buffer.
pub const FETCH_WINDOW: u64 = 16 << 20;

/// Fetch rounds that repair chunks found damaged at the end before a fetch
/// gives up. One round finds every damaged chunk, so more only helps if the
/// disk keeps damaging them.
const REPAIR_ROUNDS: usize = 3;
/// The least pace a chunk frame must move at, in bytes per second: a peer
/// that sends or reads slower than this is given up on, so nobody can hold a
/// transfer, and what waits on it, by trickling. It is 0.5 Mbit/s.
pub const MIN_RATE: u64 = 64 * 1024;
/// The time any one frame gets, however small.
const FRAME_FLOOR: Duration = Duration::from_secs(5);

/// Why a bulk transfer failed.
#[derive(Debug, Error)]
pub enum BulkError {
    #[error(transparent)]
    Net(#[from] NetError),
    #[error("the connection failed: {0}")]
    Connection(#[from] quinn::ConnectionError),
    #[error("the peer broke the bulk protocol: {0}")]
    Violation(&'static str),
    #[error("the peer sent nothing for {0:?}")]
    Idle(Duration),
    #[error("the peer no longer has the snapshot")]
    Unavailable,
    #[error("the peer's manifest is not valid: {0}")]
    Manifest(#[from] ManifestError),
    #[error("the peer sent the manifest of another snapshot")]
    WrongManifest,
    #[error(transparent)]
    Sink(#[from] SinkError),
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error("a store task failed: {0}")]
    Task(String),
}

impl BulkError {
    /// Whether the peer sent something it must not have, as opposed to the
    /// transfer failing for other reasons. A peer that broke the protocol
    /// deserves `PROTOCOL_VIOLATION`.
    pub fn is_violation(&self) -> bool {
        match self {
            Self::Violation(_) | Self::Manifest(_) | Self::WrongManifest => true,
            Self::Net(error) => !error.is_disconnect(),
            Self::Sink(error) => matches!(
                error,
                SinkError::Rejected { .. } | SinkError::NotInManifest(_)
            ),
            _ => false,
        }
    }
}

/// What a finished fetch does with the snapshot.
#[derive(Debug, Clone)]
pub enum Completion {
    /// Assemble the save file at this path, atomically.
    File(PathBuf),
    /// Keep it in the store only: a server passing it on.
    Retain,
}

/// What a serve sent.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Served {
    pub chunks: u64,
    /// Compressed bytes of the chunk frames.
    pub bytes: u64,
}

/// The wire name of a snapshot.
pub fn snapshot_id(id: &ManifestId) -> SnapshotId {
    SnapshotId(FixedBytes(id.0))
}

/// The store's name of a snapshot.
pub fn manifest_id(id: &SnapshotId) -> ManifestId {
    ManifestId(id.0.0)
}

fn chunk_hash(id: &ChunkId) -> ChunkHash {
    ChunkHash(FixedBytes(id.0))
}

fn chunk_id(hash: &ChunkHash) -> ChunkId {
    ChunkId(hash.0.0)
}

/// Serves `manifest` from `store` until the fetching side finishes its half
/// of the stream. Anything outside the manifest is a violation. A chunk the
/// store cannot produce ends the stream with `Unavailable`.
pub async fn serve(
    send: &mut SendStream,
    recv: &mut RecvStream,
    store: &ChunkStore,
    manifest: &Manifest,
    idle: Duration,
) -> Result<Served, BulkError> {
    let listed: HashSet<ChunkId> = manifest.chunks().iter().map(|entry| entry.id).collect();
    let manifest_bytes = manifest.to_bytes();
    let mut served = Served::default();
    loop {
        let request = match within(
            idle,
            read_message::<BulkRequest>(recv, BULK_REQUEST_MAX_FRAME),
        )
        .await?
        {
            Ok(request) => request,
            // The fetching side is done, or gone.
            Err(error) if error.is_disconnect() => return Ok(served),
            Err(error) => return Err(error.into()),
        };
        match request {
            BulkRequest::Manifest => {
                let response = BulkResponse::Manifest {
                    bytes: manifest_bytes.clone(),
                };
                within(
                    idle,
                    write_message(send, &response, BULK_RESPONSE_MAX_FRAME),
                )
                .await??;
            }
            BulkRequest::Chunks { ids } => {
                if ids.is_empty() || ids.len() > MAX_CHUNKS_PER_REQUEST {
                    return Err(BulkError::Violation("a chunk request of the wrong size"));
                }
                let ids: Vec<ChunkId> = ids.iter().map(chunk_id).collect();
                if !ids.iter().all(|id| listed.contains(id)) {
                    return Err(BulkError::Violation(
                        "a request for a chunk not in the manifest",
                    ));
                }
                for id in ids {
                    let store = store.clone();
                    let frame = match blocking(move || store.read_compressed(&id)).await? {
                        Ok(frame) => frame,
                        Err(error) => {
                            // A missing or damaged chunk: this copy of the
                            // snapshot is no good. Tell the peer why the
                            // stream ends.
                            let _ = within(
                                idle,
                                write_message(
                                    send,
                                    &BulkResponse::Unavailable,
                                    BULK_RESPONSE_MAX_FRAME,
                                ),
                            )
                            .await;
                            let _ = send.finish();
                            return Err(error.into());
                        }
                    };
                    served.chunks += 1;
                    served.bytes += frame.len() as u64;
                    let deadline = frame_time(frame.len() as u64, idle);
                    let response = BulkResponse::Chunk {
                        id: chunk_hash(&id),
                        frame,
                    };
                    within(
                        deadline,
                        write_message(send, &response, BULK_RESPONSE_MAX_FRAME),
                    )
                    .await??;
                }
            }
        }
    }
}

/// Fetches the snapshot `expected` into `store` and completes it as asked,
/// reporting progress as chunks arrive. Returns its manifest. Finishes this
/// side of the stream when done.
pub async fn fetch(
    send: &mut SendStream,
    recv: &mut RecvStream,
    store: &ChunkStore,
    expected: &ManifestId,
    completion: Completion,
    idle: Duration,
    mut progress: impl FnMut(Progress),
) -> Result<Manifest, BulkError> {
    within(
        idle,
        write_message(send, &BulkRequest::Manifest, BULK_REQUEST_MAX_FRAME),
    )
    .await??;
    let bytes = match next_response(recv, idle).await? {
        BulkResponse::Manifest { bytes } => bytes,
        BulkResponse::Unavailable => return Err(BulkError::Unavailable),
        BulkResponse::Chunk { .. } => {
            return Err(BulkError::Violation("a chunk before the manifest"));
        }
    };
    if bytes.len() > MAX_MANIFEST_LEN {
        return Err(BulkError::Violation("an oversized manifest"));
    }
    let manifest = blocking(move || Manifest::from_bytes(&bytes)).await??;
    if manifest.id() != *expected {
        return Err(BulkError::WrongManifest);
    }
    let sink = {
        let store = store.clone();
        let manifest = manifest.clone();
        blocking(move || open_sink(&store, manifest)).await??
    };
    progress(sink.progress());
    match fetch_into(send, recv, sink, &completion, idle, &mut progress).await {
        Ok(()) => {
            let _ = send.finish();
            Ok(manifest)
        }
        Err(failure) => {
            let (sink, error) = *failure;
            // Leave nothing pinned: the chunks already stored stay until the
            // next garbage collection, and count for another attempt.
            if let Some(sink) = sink {
                let _ = blocking(move || sink.abandon()).await;
            }
            Err(error)
        }
    }
}

/// A failed fetch, with the sink if it still has it.
type Failed = Box<(Option<ChunkSink>, BulkError)>;

fn failed(sink: Option<ChunkSink>, error: BulkError) -> Failed {
    Box::new((sink, error))
}

/// Fetches what the sink lacks and completes it, repairing damage found at
/// the end.
async fn fetch_into(
    send: &mut SendStream,
    recv: &mut RecvStream,
    mut sink: ChunkSink,
    completion: &Completion,
    idle: Duration,
    progress: &mut impl FnMut(Progress),
) -> Result<(), Failed> {
    let mut round = 0;
    loop {
        round += 1;
        sink = fetch_missing(send, recv, sink, idle, progress).await?;
        let completion = completion.clone();
        let (returned, finished) = blocking(move || {
            let finished = match &completion {
                Completion::File(path) => sink.finish(path),
                Completion::Retain => sink.retain(),
            };
            (sink, finished)
        })
        .await
        .map_err(|error| failed(None, error))?;
        sink = returned;
        match finished {
            Ok(()) => return Ok(()),
            // Damaged chunks were dropped and are missing again.
            Err(SinkError::Store(StoreError::Corrupt { .. })) if round < REPAIR_ROUNDS => {}
            Err(error) => return Err(failed(Some(sink), error.into())),
        }
    }
}

/// Opens the transfer of `manifest`, or picks it up where an earlier one
/// in this store stopped.
fn open_sink(store: &ChunkStore, manifest: Manifest) -> Result<ChunkSink, SinkError> {
    let id = manifest.id();
    if store.pending()?.contains(&id) {
        match ChunkSink::resume(store, &id) {
            Ok(sink) => return Ok(sink),
            // The saved state is damaged: start over from the manifest.
            Err(SinkError::Store(StoreError::DamagedRecord { .. })) => {}
            Err(error) => return Err(error),
        }
    }
    ChunkSink::open(store, manifest)
}

/// Requests every chunk the sink lacks and feeds the answers to it. Each
/// answer must arrive at [`MIN_RATE`] for its size.
async fn fetch_missing(
    send: &mut SendStream,
    recv: &mut RecvStream,
    mut sink: ChunkSink,
    idle: Duration,
    progress: &mut impl FnMut(Progress),
) -> Result<ChunkSink, Failed> {
    let mut wanted = sink.missing().into_iter().peekable();
    // Requested and not yet answered, in the order the answers come.
    let mut outstanding: VecDeque<(ChunkId, u64)> = VecDeque::new();
    let mut outstanding_bytes = 0;
    loop {
        while outstanding_bytes < FETCH_WINDOW && wanted.peek().is_some() {
            let mut ids = Vec::new();
            while ids.len() < MAX_CHUNKS_PER_REQUEST && outstanding_bytes < FETCH_WINDOW {
                let Some(entry) = wanted.next() else {
                    break;
                };
                ids.push(chunk_hash(&entry.id));
                outstanding.push_back((entry.id, u64::from(entry.len)));
                outstanding_bytes += u64::from(entry.len);
            }
            let request = BulkRequest::Chunks { ids };
            let sent = within(idle, write_message(send, &request, BULK_REQUEST_MAX_FRAME)).await;
            match sent {
                Ok(Ok(())) => {}
                Ok(Err(error)) => return Err(failed(Some(sink), error.into())),
                Err(error) => return Err(failed(Some(sink), error)),
            }
        }
        let Some((due, len)) = outstanding.pop_front() else {
            return Ok(sink);
        };
        outstanding_bytes -= len;
        let (id, frame) = match next_response(recv, frame_time(len, idle)).await {
            Ok(BulkResponse::Chunk { id, frame }) => (chunk_id(&id), frame),
            Ok(BulkResponse::Unavailable) => {
                return Err(failed(Some(sink), BulkError::Unavailable));
            }
            Ok(BulkResponse::Manifest { .. }) => {
                return Err(failed(
                    Some(sink),
                    BulkError::Violation("a manifest in place of a chunk"),
                ));
            }
            Err(error) => return Err(failed(Some(sink), error)),
        };
        if id != due {
            return Err(failed(
                Some(sink),
                BulkError::Violation("a chunk out of order"),
            ));
        }
        let (returned, put) = blocking(move || {
            let put = sink.put(&id, &frame);
            (sink, put)
        })
        .await
        .map_err(|error| failed(None, error))?;
        sink = returned;
        match put {
            Ok(done) => progress(done),
            Err(error) => return Err(failed(Some(sink), error.into())),
        }
    }
}

/// The time a frame of `len` bytes gets at [`MIN_RATE`], at least
/// [`FRAME_FLOOR`] and at most `idle`.
fn frame_time(len: u64, idle: Duration) -> Duration {
    Duration::from_millis(len.saturating_mul(1000) / MIN_RATE)
        .max(FRAME_FLOOR)
        .min(idle.max(FRAME_FLOOR))
}

async fn next_response(recv: &mut RecvStream, idle: Duration) -> Result<BulkResponse, BulkError> {
    Ok(within(idle, read_message(recv, BULK_RESPONSE_MAX_FRAME)).await??)
}

/// Runs `work`, failing if it takes longer than `idle`.
async fn within<T>(idle: Duration, work: impl Future<Output = T>) -> Result<T, BulkError> {
    tokio::time::timeout(idle, work)
        .await
        .map_err(|_| BulkError::Idle(idle))
}

async fn blocking<T: Send + 'static>(
    work: impl FnOnce() -> T + Send + 'static,
) -> Result<T, BulkError> {
    tokio::task::spawn_blocking(work)
        .await
        .map_err(|error| BulkError::Task(error.to_string()))
}
