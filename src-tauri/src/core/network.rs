/// TCP-based network transport for sharing input events between machines.
///
/// Architecture:
/// - Host runs a TCP server on port 24800
/// - Clients connect to the host
/// - Events flow bidirectionally over each connection
/// - Each connection gets a read task and a write task (tokio MPSC for outbound)

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{broadcast, mpsc, Mutex};

use super::protocol::{EventCodec, InputEvent};

/// Default port for ShareMouse communication.
pub const DEFAULT_PORT: u16 = 24800;

/// Maximum event payload size (1 MB — generous for clipboard, but prevents abuse).
const MAX_EVENT_SIZE: usize = 1024 * 1024;

/// Unique ID for a connected peer.
pub type PeerId = uuid::Uuid;

/// Message from the network layer to the core logic.
#[derive(Debug, Clone)]
pub enum NetworkMessage {
    /// A new peer connected
    PeerConnected {
        id: PeerId,
        addr: SocketAddr,
    },
    /// A peer disconnected
    PeerDisconnected {
        id: PeerId,
    },
    /// Received an input event from a peer
    Event {
        from: PeerId,
        event: InputEvent,
    },
    /// Server started listening
    Listening {
        addr: SocketAddr,
    },
    /// Connection error
    Error {
        message: String,
    },
}

/// The network hub — shared state for all connections.
pub struct NetworkHub {
    /// Channel to broadcast events to the core logic.
    outbound_tx: broadcast::Sender<NetworkMessage>,

    /// Per-peer write channels: send InputEvent into this to write to the peer.
    peer_writes: Arc<Mutex<HashMap<PeerId, mpsc::Sender<InputEvent>>>>,

    /// Peers that are currently connected.
    peers: Arc<Mutex<HashMap<PeerId, SocketAddr>>>,
}

impl NetworkHub {
    /// Create a new network hub.
    pub fn new() -> (Self, broadcast::Receiver<NetworkMessage>) {
        let (tx, rx) = broadcast::channel(256);
        let hub = Self {
            outbound_tx: tx,
            peer_writes: Arc::new(Mutex::new(HashMap::new())),
            peers: Arc::new(Mutex::new(HashMap::new())),
        };
        (hub, rx)
    }

    /// Get a fresh receiver for network messages (each consumer needs its own).
    pub fn subscribe(&self) -> broadcast::Receiver<NetworkMessage> {
        self.outbound_tx.subscribe()
    }

    /// Start a TCP server and accept connections.
    pub async fn start_server(&self, port: u16) -> anyhow::Result<()> {
        let listener = TcpListener::bind(("0.0.0.0", port)).await?;
        let local_addr = listener.local_addr()?;

        self.outbound_tx
            .send(NetworkMessage::Listening { addr: local_addr })
            .ok();

        log::info!("Server listening on {}", local_addr);

        loop {
            let (stream, addr) = listener.accept().await?;
            log::info!("New connection from {}", addr);
            let peer_id = PeerId::new_v4();
            let outbound_tx = self.outbound_tx.clone();
            let peer_writes = self.peer_writes.clone();
            let peers = self.peers.clone();

            outbound_tx
                .send(NetworkMessage::PeerConnected {
                    id: peer_id,
                    addr,
                })
                .ok();

            tokio::spawn(async move {
                handle_peer(peer_id, stream, outbound_tx, peer_writes, peers).await;
            });
        }
    }

    /// Connect to a remote host as a client.
    pub async fn connect_to(&self, addr: &str) -> anyhow::Result<PeerId> {
        let stream = TcpStream::connect(addr).await?;
        let peer_addr = stream.peer_addr()?;
        let peer_id = PeerId::new_v4();

        log::info!("Connected to {}", peer_addr);

        self.outbound_tx
            .send(NetworkMessage::PeerConnected {
                id: peer_id,
                addr: peer_addr,
            })
            .ok();

        let outbound_tx = self.outbound_tx.clone();
        let peer_writes = self.peer_writes.clone();
        let peers = self.peers.clone();

        // For client connections, we run the peer handler in background
        tokio::spawn(async move {
            handle_peer(peer_id, stream, outbound_tx, peer_writes, peers).await;
        });

        Ok(peer_id)
    }

    /// Send an event to a specific peer.
    pub async fn send_to(&self, peer_id: &PeerId, event: InputEvent) -> anyhow::Result<()> {
        let writes = self.peer_writes.lock().await;
        if let Some(tx) = writes.get(peer_id) {
            tx.send(event).await?;
        }
        Ok(())
    }

    /// Broadcast an event to all connected peers.
    pub async fn broadcast(&self, event: InputEvent) {
        let writes = self.peer_writes.lock().await;
        for (_id, tx) in writes.iter() {
            if tx.send(event.clone()).await.is_err() {
                log::warn!("Failed to send to peer — channel closed");
            }
        }
    }

    /// Get list of connected peers.
    pub async fn get_peers(&self) -> Vec<(PeerId, SocketAddr)> {
        let peers = self.peers.lock().await;
        peers.iter().map(|(id, addr)| (*id, *addr)).collect()
    }
}

/// Handle a single peer connection (read + write loop).
async fn handle_peer(
    peer_id: PeerId,
    stream: TcpStream,
    outbound_tx: broadcast::Sender<NetworkMessage>,
    peer_writes: Arc<Mutex<HashMap<PeerId, mpsc::Sender<InputEvent>>>>,
    peers: Arc<Mutex<HashMap<PeerId, SocketAddr>>>,
) {
    let (read_half, write_half) = stream.into_split();

    // Create per-peer write channel
    let (write_tx, write_rx) = mpsc::channel::<InputEvent>(256);
    {
        let mut writes = peer_writes.lock().await;
        writes.insert(peer_id, write_tx);
    }
    {
        let mut p = peers.lock().await;
        p.insert(peer_id, read_half.peer_addr().unwrap_or_else(|_| "0.0.0.0:0".parse().unwrap()));
    }

    // Spawn write task
    let write_peer_id = peer_id;
    let write_peer_writes = peer_writes.clone();
    let write_peers = peers.clone();
    let write_outbound = outbound_tx.clone();
    let write_handle = tokio::spawn(async move {
        if let Err(e) = write_loop(write_half, write_rx).await {
            log::error!("Write error for peer {}: {}", write_peer_id, e);
        }
        // Clean up on disconnect
        log::info!("Peer {} disconnected (write side)", write_peer_id);
        write_peer_writes.lock().await.remove(&write_peer_id);
        write_peers.lock().await.remove(&write_peer_id);
        write_outbound
            .send(NetworkMessage::PeerDisconnected { id: write_peer_id })
            .ok();
    });

    // Run read loop in this task
    if let Err(e) = read_loop(read_half, peer_id, &outbound_tx).await {
        log::error!("Read error for peer {}: {}", peer_id, e);
    }

    log::info!("Peer {} disconnected (read side)", peer_id);
    peer_writes.lock().await.remove(&peer_id);
    peers.lock().await.remove(&peer_id);
    outbound_tx
        .send(NetworkMessage::PeerDisconnected { id: peer_id })
        .ok();

    // Wait for write task to finish
    write_handle.abort();
}

/// Read events from a peer connection.
async fn read_loop(
    mut read_half: tokio::net::tcp::OwnedReadHalf,
    peer_id: PeerId,
    outbound_tx: &broadcast::Sender<NetworkMessage>,
) -> anyhow::Result<()> {
    let mut buf = Vec::with_capacity(4096);
    let mut read_buf = [0u8; 4096];

    loop {
        let n = read_half.read(&mut read_buf).await?;
        if n == 0 {
            // Connection closed
            return Ok(());
        }
        buf.extend_from_slice(&read_buf[..n]);

        // Try to decode all complete events in the buffer
        let mut consumed = 0;
        while consumed < buf.len() {
            match EventCodec::decode(&buf[consumed..])? {
                Some((event, bytes)) => {
                    consumed += bytes;
                    outbound_tx
                        .send(NetworkMessage::Event {
                            from: peer_id,
                            event,
                        })
                        .ok();
                }
                None => break,
            }
        }

        // Check for oversized partial event (sanity limit)
        if buf.len() - consumed > MAX_EVENT_SIZE {
            anyhow::bail!("Event too large, possible corruption");
        }

        // Drain consumed bytes
        buf.drain(..consumed);
    }
}

/// Write events to a peer connection.
async fn write_loop(
    mut write_half: tokio::net::tcp::OwnedWriteHalf,
    mut rx: mpsc::Receiver<InputEvent>,
) -> anyhow::Result<()> {
    while let Some(event) = rx.recv().await {
        let data = EventCodec::encode(&event)?;
        write_half.write_all(&data).await?;
        write_half.flush().await?;
    }
    Ok(())
}
