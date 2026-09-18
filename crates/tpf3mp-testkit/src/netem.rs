//! A UDP proxy that adds latency, jitter and loss between clients and a
//! server, so tests run QUIC over a bad network instead of a perfect
//! loopback. Jitter reorders packets as a real path does.

use std::{
    collections::HashMap,
    io,
    net::SocketAddr,
    sync::{Arc, Mutex, PoisonError},
    time::Duration,
};

use tokio::{net::UdpSocket, task::JoinSet};

use crate::rng::SplitMix64;

#[derive(Debug, Clone, Copy)]
pub struct Impairment {
    /// One-way delay in each direction.
    pub latency: Duration,
    /// Extra delay, uniform in `0..=jitter`, per packet.
    pub jitter: Duration,
    /// Packets dropped per million, in each direction.
    pub loss_per_million: u32,
}

pub struct Netem {
    address: SocketAddr,
    task: tokio::task::JoinHandle<()>,
}

impl Netem {
    /// Starts a proxy in front of `server`. Clients connect to
    /// [`Netem::address`] instead.
    pub async fn start(server: SocketAddr, impairment: Impairment, seed: u64) -> io::Result<Self> {
        let front = Arc::new(UdpSocket::bind(("127.0.0.1", 0)).await?);
        let address = front.local_addr()?;
        let rng = Arc::new(Mutex::new(SplitMix64::new(seed)));
        let task = tokio::spawn(relay(front, server, impairment, rng));
        Ok(Self { address, task })
    }

    pub fn address(&self) -> SocketAddr {
        self.address
    }
}

impl Drop for Netem {
    fn drop(&mut self) {
        // Aborting the relay drops its JoinSet, which aborts every session.
        self.task.abort();
    }
}

type Rng = Arc<Mutex<SplitMix64>>;

async fn relay(front: Arc<UdpSocket>, server: SocketAddr, impairment: Impairment, rng: Rng) {
    let mut sessions: HashMap<SocketAddr, Arc<UdpSocket>> = HashMap::new();
    let mut backward = JoinSet::new();
    let mut buffer = vec![0; 65_536];
    loop {
        let Ok((len, client)) = front.recv_from(&mut buffer).await else {
            return;
        };
        let upstream = match sessions.get(&client) {
            Some(upstream) => Arc::clone(upstream),
            None => {
                let Ok(upstream) = open_upstream(server).await else {
                    continue;
                };
                let upstream = Arc::new(upstream);
                backward.spawn(return_path(
                    Arc::clone(&upstream),
                    Arc::clone(&front),
                    client,
                    impairment,
                    Arc::clone(&rng),
                ));
                sessions.insert(client, Arc::clone(&upstream));
                upstream
            }
        };
        let packet = buffer[..len].to_vec();
        if let Some(delay) = fate(&rng, impairment) {
            tokio::spawn(async move {
                tokio::time::sleep(delay).await;
                let _ = upstream.send(&packet).await;
            });
        }
    }
}

async fn open_upstream(server: SocketAddr) -> io::Result<UdpSocket> {
    let socket = UdpSocket::bind(("127.0.0.1", 0)).await?;
    socket.connect(server).await?;
    Ok(socket)
}

async fn return_path(
    upstream: Arc<UdpSocket>,
    front: Arc<UdpSocket>,
    client: SocketAddr,
    impairment: Impairment,
    rng: Rng,
) {
    let mut buffer = vec![0; 65_536];
    while let Ok(len) = upstream.recv(&mut buffer).await {
        let packet = buffer[..len].to_vec();
        if let Some(delay) = fate(&rng, impairment) {
            let front = Arc::clone(&front);
            tokio::spawn(async move {
                tokio::time::sleep(delay).await;
                let _ = front.send_to(&packet, client).await;
            });
        }
    }
}

/// The delay of one packet, or `None` if it is lost.
fn fate(rng: &Rng, impairment: Impairment) -> Option<Duration> {
    let mut rng = rng.lock().unwrap_or_else(PoisonError::into_inner);
    if rng.chance(impairment.loss_per_million) {
        return None;
    }
    let jitter_us = u64::try_from(impairment.jitter.as_micros()).unwrap_or(u64::MAX);
    let extra = Duration::from_micros(rng.below(jitter_us.saturating_add(1)));
    Some(impairment.latency + extra)
}
