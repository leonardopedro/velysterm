//! Collaborative editing sync primitives (C13).
//!
//! `export_delta()` produces a compact binary patch of all operations
//! since the last export. `import_delta()` applies a remote patch.
//! Two `MathDoc` instances exchanging deltas converge to identical
//! text.
//!
//! Live presence (who is here, where their caret is) rides the same
//! transport: [`PresenceStore`] is backed by Loro's ephemeral store,
//! so presence is never written into the document history and never
//! persisted — it is gossip, exactly like Lody's `presence` /
//! `session-live-status` modules. Peers exchange `encode()`d blobs
//! over the same channel that carries deltas, and a peer whose
//! heartbeat lapses past the timeout is pruned by `remove_outdated`.

use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, RwLock};

use crate::doc::MathDoc;
use loro::awareness::EphemeralStore;
use loro::{ExportMode, LoroMapValue, LoroValue};

impl MathDoc {
    /// Export all operations since the last export as a compact
    /// binary patch suitable for network transport.
    pub fn export_delta(&self) -> Vec<u8> {
        self.doc
            .export(ExportMode::all_updates())
            .expect("delta export cannot fail")
    }

    /// Import a remote delta patch, merging concurrent operations.
    pub fn import_delta(&mut self, delta: &[u8]) -> Result<(), crate::doc::DocError> {
        self.doc
            .import(delta)
            .map_err(|e| crate::doc::DocError::Loro(e.to_string()))?;
        self.mirror = self.text.to_string();
        Ok(())
    }
}

/// A live collaborator: who they are, where their caret is, and when
/// they were last heard from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Presence {
    /// Stable peer id (opaque to the transport).
    pub peer: String,
    /// Display name, as published by the peer.
    pub name: String,
    /// Caret byte offset into the document, or `None` when the peer
    /// is not currently editing the document.
    pub cursor: Option<usize>,
    /// Millisecond epoch of the peer's last heartbeat.
    pub last_seen_ms: i64,
}

/// Live presence channel for a shared document (C13).
///
/// One `PresenceStore` per peer per document. `set_name` /
/// `set_cursor` publish a heartbeat; `encode` / `encode_all` produce
/// the transport payload; `apply` merges a remote payload;
/// `remove_outdated` prunes peers whose heartbeat lapsed past
/// `timeout_ms`, and `peers` skips them even before a prune pass
/// runs.
///
/// Nothing here touches the document's CRDT history: presence is
/// ephemeral by construction and disappears when its peers stop
/// publishing.
#[derive(Debug)]
pub struct PresenceStore {
    store: EphemeralStore,
    peer: String,
    /// Local display state; shared so setters take `&self` like the
    /// underlying ephemeral store (host handlers need no `mut`).
    local: Arc<RwLock<LocalPresence>>,
    /// Millisecond of the last publish, reserved so every publish is
    /// strictly newer than the previous one (see [`Self::publish`]).
    last_set_ms: AtomicI64,
    /// The inactivity timeout, kept here so `peers` can filter
    /// expired entries without a `remove_outdated` pass having
    /// run (Loro keeps expired entries in `get_all_states` until
    /// they are purged).
    timeout_ms: i64,
}

impl Clone for PresenceStore {
    fn clone(&self) -> Self {
        Self {
            store: self.store.clone(),
            peer: self.peer.clone(),
            local: Arc::clone(&self.local),
            last_set_ms: AtomicI64::new(self.last_set_ms.load(Ordering::Relaxed)),
            timeout_ms: self.timeout_ms,
        }
    }
}

/// This peer's locally published display state.
#[derive(Debug, Clone, Default)]
struct LocalPresence {
    name: String,
    cursor: Option<usize>,
}

const KEY_NAME: &str = "name";
const KEY_CURSOR: &str = "cursor";
const KEY_SEEN: &str = "seen";

impl PresenceStore {
    /// Create a presence channel for `peer`, displayed as `name`.
    ///
    /// `timeout_ms` is the inactivity timeout: a peer that has not
    /// been heard from within this window is skipped by `encode`
    /// and pruned by `remove_outdated`.
    pub fn new(peer: impl Into<String>, name: impl Into<String>, timeout_ms: i64) -> Self {
        Self {
            store: EphemeralStore::new(timeout_ms),
            peer: peer.into(),
            local: Arc::new(RwLock::new(LocalPresence {
                name: name.into(),
                cursor: None,
            })),
            last_set_ms: AtomicI64::new(0),
            timeout_ms,
        }
    }

    /// This channel's peer id.
    pub fn peer(&self) -> &str {
        &self.peer
    }

    /// Publish the current presence state (name + caret + heartbeat).
    ///
    /// Loro's ephemeral store dedups on `apply` by the publisher's
    /// millisecond timestamp, so two publishes within the same
    /// millisecond would carry identical timestamps and the second
    /// would be silently dropped by a remote peer. To keep every
    /// publish strictly newer than the last, the next free
    /// millisecond is reserved before the store is updated (a
    /// brief busy-wait, bounded by the millisecond granularity,
    /// and only when two publishes collide).
    fn publish(&self) {
        let local = self.local.read().unwrap_or_else(|e| e.into_inner());
        let seen = self.reserve_timestamp();
        let mut fields = vec![
            (KEY_NAME.to_string(), LoroValue::from(local.name.clone())),
            (KEY_SEEN.to_string(), LoroValue::from(seen)),
        ];
        if let Some(c) = local.cursor {
            fields.push((KEY_CURSOR.to_string(), LoroValue::from(c as i64)));
        }
        self.store
            .set(&self.peer, LoroValue::Map(LoroMapValue::from(fields)));
    }

    /// Reserve a millisecond strictly greater than every previously
    /// reserved one, so the store's set-time timestamps strictly
    /// increase across publishes.
    fn reserve_timestamp(&self) -> i64 {
        loop {
            let seen = now_ms();
            let last = self.last_set_ms.load(Ordering::Relaxed);
            if seen > last {
                if self
                    .last_set_ms
                    .compare_exchange(last, seen, Ordering::Relaxed, Ordering::Relaxed)
                    .is_ok()
                {
                    return seen;
                }
            } else {
                std::hint::spin_loop();
            }
        }
    }

    /// Set this peer's display name and republish.
    pub fn set_name(&self, name: impl Into<String>) {
        self.local.write().unwrap_or_else(|e| e.into_inner()).name = name.into();
        self.publish();
    }

    /// Set this peer's caret position and republish. `None` signals
    /// the peer left the document.
    pub fn set_cursor(&self, cursor: Option<usize>) {
        self.local.write().unwrap_or_else(|e| e.into_inner()).cursor = cursor;
        self.publish();
    }

    /// Encode this peer's presence for transport.
    pub fn encode(&self) -> Vec<u8> {
        self.store.encode(&self.peer)
    }

    /// Encode every live peer's presence for transport.
    pub fn encode_all(&self) -> Vec<u8> {
        self.store.encode_all()
    }

    /// Merge a remote presence payload (from `encode`/`encode_all`).
    pub fn apply(&self, blob: &[u8]) -> Result<(), Box<str>> {
        self.store.apply(blob)
    }

    /// Prune peers whose heartbeat lapsed past the timeout.
    pub fn remove_outdated(&self) {
        self.store.remove_outdated();
    }

    /// The live peer list, self excluded, sorted by peer id.
    ///
    /// Loro's `get_all_states` returns expired entries until an
    /// explicit `remove_outdated` purges them (in Rust nothing
    /// prunes automatically), so the view filters on the
    /// published heartbeat itself: a peer not heard from within
    /// the timeout is invisible here without requiring a prune
    /// pass to have run. An entry with no heartbeat field at all
    /// counts as long dead.
    pub fn peers(&self) -> Vec<Presence> {
        let mut out: Vec<Presence> = self
            .store
            .get_all_states()
            .iter()
            .filter(|(id, _)| id.as_str() != self.peer)
            .filter_map(|(id, v)| decode_presence(id, v))
            .filter(|p| !self.expired(p.last_seen_ms))
            .collect();
        out.sort_by(|a, b| a.peer.cmp(&b.peer));
        out
    }

    /// Whether a heartbeat from `last_seen_ms` has lapsed past the
    /// timeout, mirroring Loro's `now - timestamp > timeout` expiry
    /// semantics on the same millisecond clock the heartbeat
    /// publishes.
    fn expired(&self, last_seen_ms: i64) -> bool {
        now_ms() - last_seen_ms > self.timeout_ms
    }

    /// Subscribe to this peer's own presence updates.
    ///
    /// The callback receives the encoded payload to broadcast; return
    /// `false` to unsubscribe. Lets a host push presence changes over
    /// the same socket that carries deltas.
    pub fn subscribe_local_updates(
        &self,
        callback: loro::awareness::LocalEphemeralCallback,
    ) -> loro::Subscription {
        self.store.subscribe_local_updates(callback)
    }
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

fn decode_presence(peer: &str, v: &LoroValue) -> Option<Presence> {
    let LoroValue::Map(m) = v else {
        return None;
    };
    let name = match m.get(KEY_NAME) {
        Some(LoroValue::String(s)) => s.to_string(),
        _ => return None,
    };
    let cursor = match m.get(KEY_CURSOR) {
        Some(LoroValue::I64(i)) if *i >= 0 => Some(*i as usize),
        _ => None,
    };
    let last_seen_ms = match m.get(KEY_SEEN) {
        Some(LoroValue::I64(i)) => *i,
        _ => 0,
    };
    Some(Presence {
        peer: peer.to_string(),
        name,
        cursor,
        last_seen_ms,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn two_docs_converge_after_delta_exchange() {
        let mut doc_a = MathDoc::new();
        let mut doc_b = MathDoc::new();

        doc_a.insert(0, "hello from A");
        doc_b.insert(0, "hello from B");

        let delta_a = doc_a.export_delta();
        let delta_b = doc_b.export_delta();

        doc_a.import_delta(&delta_b).unwrap();
        doc_b.import_delta(&delta_a).unwrap();

        assert_eq!(doc_a.text(), doc_b.text());
    }

    #[test]
    fn concurrent_edits_converge() {
        let mut doc_a = MathDoc::new();
        doc_a.insert(0, "shared prefix");

        let snapshot = doc_a.snapshot();
        let mut doc_b = MathDoc::from_snapshot(&snapshot).unwrap();

        doc_a.insert(doc_a.text().len(), " + A suffix");
        doc_b.insert(doc_b.text().len(), " + B suffix");

        let delta_a = doc_a.export_delta();
        let delta_b = doc_b.export_delta();

        doc_a.import_delta(&delta_b).unwrap();
        doc_b.import_delta(&delta_a).unwrap();

        assert_eq!(doc_a.text(), doc_b.text());
        let text = doc_a.text();
        assert!(text.contains("A suffix"), "text: {text}");
        assert!(text.contains("B suffix"), "text: {text}");
    }

    #[test]
    fn empty_delta_is_noop() {
        let mut doc = MathDoc::new();
        doc.insert(0, "content");
        let before = doc.text().to_string();

        let empty_doc = MathDoc::new();
        let empty_delta = empty_doc.export_delta();
        doc.import_delta(&empty_delta).unwrap();

        assert_eq!(doc.text(), before);
    }

    #[test]
    fn presence_cursor_roundtrips_between_peers() {
        let alice = PresenceStore::new("peer-a", "Alice", 60_000);
        let bob = PresenceStore::new("peer-b", "Bob", 60_000);

        alice.set_cursor(Some(12));
        bob.apply(&alice.encode()).unwrap();

        let peers = bob.peers();
        assert_eq!(peers.len(), 1, "peers: {peers:?}");
        assert_eq!(peers[0].peer, "peer-a");
        assert_eq!(peers[0].name, "Alice");
        assert_eq!(peers[0].cursor, Some(12));
        assert!(peers[0].last_seen_ms > 0);
    }

    #[test]
    fn presence_excludes_self() {
        let alice = PresenceStore::new("peer-a", "Alice", 60_000);
        let bob = PresenceStore::new("peer-b", "Bob", 60_000);

        alice.set_cursor(Some(1));
        bob.set_cursor(Some(2));
        bob.apply(&alice.encode_all()).unwrap();

        // Bob's view has only Alice; his own entry is never listed.
        assert_eq!(bob.peers().len(), 1);
        assert_eq!(bob.peers()[0].peer, "peer-a");

        // Alice sees Bob, not herself.
        alice.apply(&bob.encode_all()).unwrap();
        assert_eq!(alice.peers().len(), 1);
        assert_eq!(alice.peers()[0].peer, "peer-b");
    }

    #[test]
    fn presence_cursor_clears_on_leave() {
        let alice = PresenceStore::new("peer-a", "Alice", 60_000);
        let bob = PresenceStore::new("peer-b", "Bob", 60_000);

        alice.set_cursor(Some(5));
        bob.apply(&alice.encode()).unwrap();
        assert_eq!(bob.peers()[0].cursor, Some(5));

        alice.set_cursor(None);
        bob.apply(&alice.encode()).unwrap();
        assert_eq!(bob.peers()[0].cursor, None);
    }

    #[test]
    fn presence_merges_concurrent_updates() {
        let alice = PresenceStore::new("peer-a", "Alice", 60_000);
        let bob = PresenceStore::new("peer-b", "Bob", 60_000);

        alice.set_cursor(Some(3));
        bob.set_cursor(Some(7));
        alice.apply(&bob.encode()).unwrap();
        bob.apply(&alice.encode()).unwrap();

        assert_eq!(alice.peers().len(), 1);
        assert_eq!(alice.peers()[0].name, "Bob");
        assert_eq!(bob.peers().len(), 1);
        assert_eq!(bob.peers()[0].name, "Alice");
    }

    #[test]
    fn presence_expires_after_timeout() {
        let alice = PresenceStore::new("peer-a", "Alice", 1);
        let bob = PresenceStore::new("peer-b", "Bob", 1);

        alice.set_cursor(Some(3));
        bob.apply(&alice.encode()).unwrap();
        assert_eq!(bob.peers().len(), 1);

        // 1 ms timeout: after a short wait the stale peer is pruned.
        std::thread::sleep(std::time::Duration::from_millis(50));
        bob.remove_outdated();
        assert!(bob.peers().is_empty(), "peers: {:?}", bob.peers());
    }

    #[test]
    fn presence_peers_skip_expired_without_prune() {
        let alice = PresenceStore::new("peer-a", "Alice", 1);
        let bob = PresenceStore::new("peer-b", "Bob", 1);

        alice.set_cursor(Some(3));
        bob.apply(&alice.encode()).unwrap();
        assert_eq!(bob.peers().len(), 1);

        // After the timeout lapses the stale peer disappears from the
        // view even though no `remove_outdated()` pass has run (Loro
        // keeps expired entries in `get_all_states` until purged).
        std::thread::sleep(std::time::Duration::from_millis(50));
        assert!(bob.peers().is_empty(), "peers: {:?}", bob.peers());

        // A fresh heartbeat revives the peer.
        alice.set_cursor(Some(4));
        bob.apply(&alice.encode()).unwrap();
        assert_eq!(bob.peers().len(), 1);
        assert_eq!(bob.peers()[0].cursor, Some(4));
    }

    #[test]
    fn presence_encode_carries_only_publisher() {
        let alice = PresenceStore::new("peer-a", "Alice", 60_000);
        let carol = PresenceStore::new("peer-c", "Carol", 60_000);

        alice.set_cursor(Some(9));
        carol.apply(&alice.encode()).unwrap();
        assert_eq!(carol.peers().len(), 1);
        assert_eq!(carol.peers()[0].peer, "peer-a");
    }
}
