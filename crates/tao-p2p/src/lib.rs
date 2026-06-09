//! `tao-p2p` — minimal TCP gossip networking for block and transaction relay.
//!
//! A deliberately small, dependency-free P2P layer for the devnet: each node
//! listens on a TCP port and dials its configured bootstrap peers; every
//! established connection is bidirectional. Messages are length-prefixed
//! bincode. Inbound messages are delivered on a channel; [`Network::broadcast`]
//! fans a message out to all peers.
//!
//! Topology for M5 is a star (followers dial the miner), which sidesteps
//! multi-miner reorgs. A production node would use libp2p (gossip dedup, peer
//! discovery, NAT traversal); that is a later swap. There is no orphan buffering
//! yet — blocks are expected in order from a single miner (TCP preserves order).

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::mpsc::Sender;
use std::sync::{atomic::AtomicUsize, atomic::Ordering, Arc, Mutex};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tao_core::error::{Result, TaoError};

/// Max frame size accepted over the network.
const MAX_MSG_SIZE: usize = 8 * 1024 * 1024;

/// Per-peer identifier used by the local node for direct responses.
pub type PeerId = usize;

/// Inbound envelope with sender identity.
#[derive(Debug, Clone)]
pub struct InboundMsg {
    pub from: PeerId,
    pub msg: NetMsg,
}

/// A gossip message.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum NetMsg {
    /// A serialized `Block` (bincode of `tao_consensus::Block`).
    NewBlock(Vec<u8>),
    /// A serialized Solana `Transaction` (bincode).
    NewTx(Vec<u8>),
    /// Request a block by id (32-byte hash). A peer that has it replies with
    /// `NewBlock`. Used to backfill an orphan's missing ancestors.
    GetBlock([u8; 32]),
    /// Ask peers to announce their current DAG tips. Used for initial sync: a
    /// node with no fresh gossip still discovers the chain.
    GetTips,
    /// Announce current DAG tip block ids (response to `GetTips`). The requester
    /// `GetBlock`s any tip it doesn't have, which transitively backfills.
    Tips(Vec<[u8; 32]>),
    /// Ask a (pruned) peer for its bootstrap snapshot. Used when block backfill
    /// stalls because the needed ancestors were pruned.
    GetSnapshot,
    /// A serialized bootstrap snapshot (pruning point + base state + kept suffix),
    /// response to `GetSnapshot`. The requester adopts it if it is behind.
    Snapshot(Vec<u8>),
}

/// A handle to the gossip network: broadcast out, peers in via the inbound channel.
#[derive(Clone)]
pub struct Network {
    peers: Arc<Mutex<Vec<(PeerId, TcpStream)>>>,
}

impl Network {
    /// Start listening on `listen` and dial each `bootstrap` peer. Inbound
    /// messages from any peer are sent to `inbound`.
    pub fn start(
        listen: SocketAddr,
        bootstrap: Vec<SocketAddr>,
        inbound: Sender<InboundMsg>,
    ) -> Result<Network> {
        let peers: Arc<Mutex<Vec<(PeerId, TcpStream)>>> = Arc::new(Mutex::new(Vec::new()));
        let listener = TcpListener::bind(listen).map_err(TaoError::Io)?;
        tracing::info!(%listen, "p2p listening");
        let next_peer_id = Arc::new(AtomicUsize::new(0));

        // Accept loop.
        {
            let peers = peers.clone();
            let inbound = inbound.clone();
            let next_peer_id = next_peer_id.clone();
            std::thread::spawn(move || {
                for stream in listener.incoming() {
                    match stream {
                        Ok(s) => {
                            add_connection(s, peers.clone(), next_peer_id.clone(), inbound.clone())
                        }
                        Err(e) => tracing::warn!(error = %e, "accept failed"),
                    }
                }
            });
        }

        // Dial bootstrap peers (with retry — they may not be up yet).
        for addr in bootstrap {
            let peers = peers.clone();
            let inbound = inbound.clone();
            let next_peer_id = next_peer_id.clone();
            std::thread::spawn(move || {
                for attempt in 0..30 {
                    match TcpStream::connect_timeout(&addr, Duration::from_secs(2)) {
                        Ok(s) => {
                            tracing::info!(%addr, "dialed peer");
                            add_connection(s, peers, next_peer_id, inbound);
                            return;
                        }
                        Err(_) => {
                            std::thread::sleep(Duration::from_millis(300 * (attempt.min(5) + 1)))
                        }
                    }
                }
                tracing::warn!(%addr, "could not connect to bootstrap peer");
            });
        }

        Ok(Network { peers })
    }

    /// Send a message to every connected peer (dropping any that error).
    pub fn broadcast(&self, msg: &NetMsg) {
        let framed = match frame(msg) {
            Ok(f) => f,
            Err(e) => {
                tracing::error!(error = %e, "frame message");
                return;
            }
        };
        let mut peers = self.peers.lock().unwrap();
        peers.retain_mut(|(_, stream)| {
            stream
                .write_all(&framed)
                .and_then(|_| stream.flush())
                .is_ok()
        });
    }

    /// Send a message directly to a peer.
    pub fn send_to(&self, peer: PeerId, msg: &NetMsg) -> Result<()> {
        let framed = frame(msg)?;
        let mut peers = self.peers.lock().unwrap();
        let pos = peers
            .iter()
            .position(|(id, _)| *id == peer)
            .ok_or_else(|| TaoError::Network("unknown peer".into()))?;
        if peers[pos]
            .1
            .write_all(&framed)
            .and_then(|_| peers[pos].1.flush())
            .is_ok()
        {
            Ok(())
        } else {
            peers.remove(pos);
            Err(TaoError::Network("peer write failed".into()))
        }
    }

    /// Return one connected peer id for targeted requests.
    pub fn any_peer(&self) -> Option<PeerId> {
        self.peers.lock().unwrap().first().map(|(id, _)| *id)
    }

    /// Number of currently connected peers.
    pub fn peer_count(&self) -> usize {
        self.peers.lock().unwrap().len()
    }
}

fn add_connection(
    stream: TcpStream,
    peers: Arc<Mutex<Vec<(PeerId, TcpStream)>>>,
    next_peer_id: Arc<AtomicUsize>,
    inbound: Sender<InboundMsg>,
) {
    let reader = match stream.try_clone() {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!(error = %e, "clone stream");
            return;
        }
    };
    let peer = next_peer_id.fetch_add(1, Ordering::SeqCst);
    peers.lock().unwrap().push((peer, stream));
    std::thread::spawn(move || read_loop(reader, peer, peers, inbound));
}

fn read_loop(
    mut stream: TcpStream,
    peer: PeerId,
    peers: Arc<Mutex<Vec<(PeerId, TcpStream)>>>,
    inbound: Sender<InboundMsg>,
) {
    loop {
        match read_frame(&mut stream) {
            Ok(msg) => {
                if inbound.send(InboundMsg { from: peer, msg }).is_err() {
                    break; // consumer gone
                }
            }
            Err(_) => {
                let mut peers = peers.lock().unwrap();
                peers.retain(|(id, _)| *id != peer);
                break;
            }
        }
    }
}

fn frame(msg: &NetMsg) -> Result<Vec<u8>> {
    let body = bincode::serialize(msg).map_err(|e| TaoError::Network(e.to_string()))?;
    let len: u32 = body
        .len()
        .try_into()
        .map_err(|_| TaoError::Network("message too large".into()))?;
    let mut out = Vec::with_capacity(4 + body.len());
    out.extend_from_slice(&len.to_be_bytes());
    out.extend_from_slice(&body);
    Ok(out)
}

fn read_frame(stream: &mut TcpStream) -> Result<NetMsg> {
    let mut len_buf = [0u8; 4];
    stream.read_exact(&mut len_buf).map_err(TaoError::Io)?;
    let len = u32::from_be_bytes(len_buf) as usize;
    if len > MAX_MSG_SIZE {
        return Err(TaoError::Network(format!(
            "frame too large: {len} > {MAX_MSG_SIZE}"
        )));
    }
    let mut body = vec![0u8; len];
    stream.read_exact(&mut body).map_err(TaoError::Io)?;
    bincode::deserialize(&body).map_err(|e| TaoError::Network(e.to_string()))
}
