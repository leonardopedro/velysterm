//! Real presence transport (C13): a std-only TCP channel between
//! mathed instances carrying presence blobs over the same frame
//! format that would carry document deltas.
//!
//! Framing: `[tag: u8][len: u32 big-endian][payload]`. `TAG_PRESENCE`
//! (`b'P'`) marks a presence blob (a `PresenceStore::encode()`
//! payload); the tag leaves room for a future `b'D'` delta frame on
//! the same socket without a breaking change.
//!
//! Two background threads per connection: a reader that unframes the
//! wire into an inbox (`recv_frame` drains it), and a writer that
//! frames the outbox (`send_frame` feeds it). `--listen` accepts the
//! first peer in the background, so the editor starts immediately;
//! `--connect` dials a listener. The host drains the inbox in
//! `sync_presence` and feeds the payloads to `PresenceStore::apply`.
//! Dropping the transport sets a shutdown flag and closes the socket,
//! so the peer observes EOF.

use std::io::{Read, Write};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::sync::{Arc, Mutex};
use std::thread;

use bevy::prelude::Resource;

/// A presence blob frame (`PresenceStore::encode()` payload).
pub const TAG_PRESENCE: u8 = b'P';

const HEADER_LEN: usize = 5;

/// A live TCP presence channel to one peer.
///
/// `send_frame` / `recv_frame` are the host surface; the threads are
/// detached and the socket is kept alive for the transport's
/// lifetime. `connected` flips to `false` when the reader hits EOF
/// (peer gone).
#[derive(Debug, Resource)]
pub struct PresenceTransport {
    /// Frames from the wire (tag, payload), drained by the host.
    /// Wrapped so the resource is `Sync` (a bare `Receiver` is not).
    inbox: Mutex<Receiver<(u8, Vec<u8>)>>,
    /// The outbound half of the inbox channel (cloned into reader
    /// threads; kept here so `listen`'s accept thread can spawn a
    /// reader over the same channel).
    inbox_tx: Sender<(u8, Vec<u8>)>,
    /// Frames to write to the wire.
    outbox: Sender<(u8, Vec<u8>)>,
    /// The outbound half of the outbox channel, taken by the writer
    /// thread exactly once (immediately on connect, on accept for a
    /// listener).
    outbox_rx: Mutex<Option<Receiver<(u8, Vec<u8>)>>>,
    /// Our end's address.
    local_addr: String,
    /// Peer's address, once connected (empty until a listener
    /// accepts).
    peer: Arc<Mutex<String>>,
    /// The live socket (a listener transport fills it on accept).
    /// Held so `Drop` can close it and unblock the reader thread.
    stream: Arc<Mutex<Option<Arc<TcpStream>>>>,
    /// Whether the reader thread is still receiving.
    connected: Arc<AtomicBool>,
    /// Set on drop; the reader/writer threads exit.
    shutdown: Arc<AtomicBool>,
}

impl PresenceTransport {
    /// Dial `addr` (`host:port`) as a client. Fails if nothing
    /// listens.
    pub fn connect(addr: &str) -> std::io::Result<Self> {
        let stream = Arc::new(TcpStream::connect(addr)?);
        let local = stream.local_addr()?.to_string();
        let peer = stream.peer_addr()?.to_string();
        let t = Self::from_stream(Arc::clone(&stream), local, peer);
        t.spawn_threads();
        Ok(t)
    }

    /// Bind `addr` and accept the first peer in the background. The
    /// editor starts immediately; `connected()` reports `false` until
    /// a peer arrives. Fails if the bind fails (e.g. port in
    /// use).
    pub fn listen(addr: &str) -> std::io::Result<Self> {
        let listener = TcpListener::bind(addr)?;
        let local = listener.local_addr()?.to_string();
        let t = Self::from_stream(Arc::new(placeholder_stream()), local, String::new());
        let outbox_rx = Arc::new(Mutex::new(None));
        *outbox_rx.lock().unwrap_or_else(|e| e.into_inner()) = Some(t.take_outbox_rx());
        let inbox_tx = t.inbox_tx.clone();
        let connected = Arc::clone(&t.connected);
        let shutdown = Arc::clone(&t.shutdown);
        let peer = Arc::clone(&t.peer);
        let stream_slot = Arc::clone(&t.stream);
        thread::spawn(move || match listener.accept() {
            Ok((stream, _)) => {
                let addr = stream
                    .peer_addr()
                    .map(|a| a.to_string())
                    .unwrap_or_default();
                *peer.lock().unwrap_or_else(|e| e.into_inner()) = addr;
                let stream = Arc::new(stream);
                *stream_slot.lock().unwrap_or_else(|e| e.into_inner()) = Some(Arc::clone(&stream));
                let outbox_rx = outbox_rx
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .take()
                    .expect("the writer thread is spawned exactly once");
                spawn_reader_writer(stream, outbox_rx, inbox_tx, connected, shutdown);
            }
            Err(_) => {
                connected.store(false, Ordering::Relaxed);
            }
        });
        Ok(t)
    }

    fn from_stream(stream: Arc<TcpStream>, local_addr: String, peer: String) -> Self {
        let (inbox_tx, inbox) = mpsc::channel();
        let (outbox, outbox_rx) = mpsc::channel::<(u8, Vec<u8>)>();
        Self {
            inbox: Mutex::new(inbox),
            inbox_tx,
            outbox,
            outbox_rx: Mutex::new(Some(outbox_rx)),
            local_addr,
            peer: Arc::new(Mutex::new(peer)),
            stream: Arc::new(Mutex::new(Some(stream))),
            connected: Arc::new(AtomicBool::new(true)),
            shutdown: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Take the writer's outbox receiver (one writer per transport).
    fn take_outbox_rx(&self) -> Receiver<(u8, Vec<u8>)> {
        self.outbox_rx
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take()
            .expect("the writer thread is spawned exactly once")
    }

    fn spawn_threads(&self) {
        let stream = self
            .stream
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
            .expect("a connected transport owns its socket");
        spawn_reader_writer(
            stream,
            self.take_outbox_rx(),
            self.inbox_tx.clone(),
            Arc::clone(&self.connected),
            Arc::clone(&self.shutdown),
        );
    }

    /// Send one frame to the peer (buffered; the writer thread
    /// flushes).
    pub fn send_frame(&self, tag: u8, payload: &[u8]) {
        let _ = self.outbox.send((tag, payload.to_vec()));
    }

    /// Convenience: send a presence blob.
    pub fn send_presence(&self, blob: &[u8]) {
        self.send_frame(TAG_PRESENCE, blob);
    }

    /// Drain one inbound frame, if any.
    pub fn recv_frame(&self) -> Option<(u8, Vec<u8>)> {
        match self
            .inbox
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .try_recv()
        {
            Ok(f) => Some(f),
            Err(TryRecvError::Empty) | Err(TryRecvError::Disconnected) => None,
        }
    }

    /// Whether the reader thread is still receiving from the peer.
    pub fn connected(&self) -> bool {
        self.connected.load(Ordering::Relaxed)
    }

    /// Our end's address.
    pub fn local_addr(&self) -> &str {
        &self.local_addr
    }

    /// The peer's address (empty until a listener accepts one).
    pub fn peer(&self) -> String {
        self.peer.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }
}

impl Drop for PresenceTransport {
    fn drop(&mut self) {
        // Signal the threads and close the socket so a blocked reader
        // unblocks and the peer observes EOF.
        self.shutdown.store(true, Ordering::Relaxed);
        if let Some(s) = self
            .stream
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
        {
            let _ = s.shutdown(Shutdown::Both);
        }
    }
}

/// A connected loopback socket standing in for a not-yet-accepted
/// listener transport (so `Drop` always has a socket to close).
fn placeholder_stream() -> TcpStream {
    let listener = TcpListener::bind("127.0.0.1:0").expect("placeholder bind must succeed");
    let addr = listener.local_addr().unwrap();
    let stream = TcpStream::connect(addr).expect("placeholder connect");
    let _ = listener.accept();
    stream
}

/// Spawn the reader (wire → inbox) and writer (outbox → wire)
/// threads.
fn spawn_reader_writer(
    stream: Arc<TcpStream>,
    outbox_rx: Receiver<(u8, Vec<u8>)>,
    inbox_tx: Sender<(u8, Vec<u8>)>,
    connected: Arc<AtomicBool>,
    shutdown: Arc<AtomicBool>,
) {
    let read_half = stream
        .try_clone()
        .expect("TcpStream::try_clone must succeed");
    let write_half = stream
        .try_clone()
        .expect("TcpStream::try_clone must succeed");

    // Writer: frame outbox entries and flush; exit on shutdown.
    let writer_shutdown = Arc::clone(&shutdown);
    thread::spawn(move || {
        let mut w = write_half;
        while let Ok((tag, payload)) = outbox_rx.recv() {
            if writer_shutdown.load(Ordering::Relaxed) {
                break;
            }
            let mut frame = Vec::with_capacity(HEADER_LEN + payload.len());
            frame.push(tag);
            frame.extend_from_slice(&(payload.len() as u32).to_be_bytes());
            frame.extend_from_slice(&payload);
            if w.write_all(&frame).is_err() || w.flush().is_err() {
                break;
            }
        }
    });

    // Reader: unframe into the inbox; EOF flips `connected` off.
    let reader_shutdown = Arc::clone(&shutdown);
    thread::spawn(move || {
        let mut r = read_half;
        let mut header = [0u8; HEADER_LEN];
        loop {
            if reader_shutdown.load(Ordering::Relaxed) {
                break;
            }
            if r.read_exact(&mut header).is_err() {
                connected.store(false, Ordering::Relaxed);
                break;
            }
            let tag = header[0];
            let len = u32::from_be_bytes([header[1], header[2], header[3], header[4]]) as usize;
            let mut payload = vec![0u8; len];
            if r.read_exact(&mut payload).is_err() {
                connected.store(false, Ordering::Relaxed);
                break;
            }
            if inbox_tx.send((tag, payload)).is_err() {
                break;
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Pick a free loopback port.
    fn free_port() -> String {
        let probe = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = probe.local_addr().unwrap().to_string();
        drop(probe);
        addr
    }

    fn wait_until(mut f: impl FnMut() -> bool) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            if f() {
                return;
            }
            if std::time::Instant::now() >= deadline {
                panic!("condition not reached within 5s");
            }
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
    }

    /// Two transports over a loopback connection exchange presence
    /// blobs in both directions; dropping the client closes the
    /// socket and the server observes EOF.
    #[test]
    fn presence_roundtrip_over_loopback() {
        let addr = free_port();
        let server = PresenceTransport::listen(&addr).unwrap();
        let client = PresenceTransport::connect(&addr).unwrap();

        // The server accepts in the background; `connected` starts
        // true and flips false only on EOF, so wait for the
        // accept to land by polling until the peer address is
        // recorded.
        wait_until(|| !server.peer().is_empty());
        assert!(!server.peer().is_empty(), "server must accept the client");

        client.send_presence(b"hello-peer");
        let mut got: Option<(u8, Vec<u8>)> = None;
        wait_until(|| {
            got = server.recv_frame();
            got.is_some()
        });
        let (tag, payload) = got.expect("server receives the frame");
        assert_eq!(tag, TAG_PRESENCE);
        assert_eq!(payload, b"hello-peer");

        // Echo back and verify the client side receives it.
        server.send_frame(tag, &payload);
        let mut echoed: Option<(u8, Vec<u8>)> = None;
        wait_until(|| {
            echoed = client.recv_frame();
            echoed.is_some()
        });
        assert_eq!(echoed.unwrap().1, b"hello-peer");

        // Dropping the client (socket close) flips the server's flag.
        drop(client);
        wait_until(|| !server.connected());
        assert!(!server.connected(), "EOF must be reported");
    }

    /// A bad address fails fast.
    #[test]
    fn connect_refused_fails() {
        let addr = free_port();
        assert!(PresenceTransport::connect(&addr).is_err());
    }
}
