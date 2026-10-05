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
        // `saturating_sub`: `last_seen_ms` comes from a peer over the wire and
        // only the *cursor* is validated on decode. A peer sending
        // `i64::MIN` made `now_ms() - last_seen_ms` overflow — a debug panic, and
        // in release a wrap to a large negative, which reads as "not expired"
        // and so pins a hostile peer as permanently live. Saturating gives
        // i64::MAX, which is unambiguously long expired.
        now_ms().saturating_sub(last_seen_ms) > self.timeout_ms
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

    /// A peer-supplied timestamp must not be able to overflow the expiry
    /// subtraction.
    ///
    /// `decode_presence` validates the *cursor* but copies `seen` verbatim from
    /// the wire. `i64::MIN` therefore reached `now_ms() - last_seen_ms`: a debug
    /// panic, and in release a wrap to a large negative, which compares as "not
    /// expired" — so a hostile peer could pin itself as permanently live. The
    /// saturating form reports it as long expired instead.
    #[test]
    fn an_extreme_peer_timestamp_cannot_overflow_the_expiry_check() {
        let store = PresenceStore::new("peer-a", "Alice", 60_000);
        // A far-past timestamp saturates to i64::MAX, which is unambiguously
        // expired. Before the fix these overflowed: a debug panic, and in release
        // a wrap to a large negative that compared as "not expired".
        for hostile in [i64::MIN, i64::MIN + 1, -1, 0] {
            assert!(
                store.expired(hostile),
                "{hostile} is in the past and must read as expired"
            );
        }
        // A far-*future* timestamp saturates the other way and must not read as
        // expired, or a peer could pin itself live just as easily.
        assert!(
            !store.expired(i64::MAX),
            "a future timestamp must not read as expired"
        );
    }
}

// ── G5: write-time conflict detection ────────────────────────────────────
//
// ## The problem with a CRDT here
//
// `import_delta` merges concurrent edits, and the convergence tests prove it
// works. That is exactly why it is a problem: **a converged document is not
// necessarily a correct one.** Two agents that both rewrite the same block get a
// deterministic merge, the CRDT is happy, and nobody is told. The text is
// consistent and the meaning may be nonsense.
//
// The related-work pattern (STORM and friends) is to warn at *write* time
// rather than to fix it at read time. The block ranges are the unit, because a
// block is what a reader thinks in -- "this derivation", "this figure" -- not a
// byte range.
//
// ## Why the sender has to declare its footprint
//
// The receiver cannot work this out alone. Loro's wire format is a set of
// operations; it does not say *which blocks of the document* those operations
// landed in, and the offsets they carried are stale by the time they arrive. Any
// conflict check that does not involve the sender declaring what it touched is
// guessing.
//
// So [`DeltaPacket`] carries that declaration alongside the bytes. It is
// metadata, not a second source of truth: if a peer omits it, the import is
// treated as undeclared rather than clean.
//
// ## Applied anyway, then reported
//
// A conflict **does not block the import**. Refusing would leave the two
// documents diverged, which is worse than a reported merge: the next exchange
// would produce a different result and the divergence would be silent. The
// import happens, and [`ImportOutcome::Conflicted`] says what overlapped so the
// worker can re-check its acceptance criteria and re-merge (the cooperation
// loop's last step). Blocking is the operator's call, not this function's.

/// A block range in byte offsets, as `[start, end)`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, serde::Serialize, serde::Deserialize)]
pub struct BlockRange {
    pub start: usize,
    pub end: usize,
}

impl BlockRange {
    pub fn new(start: usize, end: usize) -> BlockRange {
        BlockRange { start, end }
    }

    /// Whether two ranges share at least one byte.
    ///
    /// Empty ranges never overlap: a zero-width range is an insertion point, and
    /// two insertions at the same point do not conflict with each other. An
    /// insertion *adjacent to* an edit does, which is why the boundary is
    /// inclusive on both ends of the other range -- see [`overlaps_with`].
    pub fn overlaps(&self, other: &BlockRange) -> bool {
        if self.is_empty() || other.is_empty() {
            return false;
        }
        self.start < other.end && other.start < self.end
    }

    pub fn is_empty(&self) -> bool {
        self.end <= self.start
    }

    /// Overlap under the stricter rule an edit/insert pair needs.
    ///
    /// An insertion at the very start or very end of a range someone else edited
    /// is a conflict in practice -- appending inside a derivation being rewritten
    /// -- even though the byte ranges are disjoint.
    pub fn overlaps_with(&self, other: &BlockRange) -> bool {
        if self.overlaps(other) {
            return true;
        }
        if self.is_empty() && !other.is_empty() {
            return other.start <= self.start && self.start <= other.end;
        }
        if other.is_empty() && !self.is_empty() {
            return self.start <= other.start && other.start <= self.end;
        }
        false
    }

    fn union(&self, other: &BlockRange) -> BlockRange {
        BlockRange::new(self.start.min(other.start), self.end.max(other.end))
    }
}

/// Do two sets of ranges overlap under [`BlockRange::overlaps_with`]?
pub fn footprints_overlap(a: &[BlockRange], b: &[BlockRange]) -> bool {
    a.iter().any(|x| b.iter().any(|y| x.overlaps_with(y)))
}

/// The pairs that overlap, for a report a human can act on.
pub fn overlapping_pairs(a: &[BlockRange], b: &[BlockRange]) -> Vec<(BlockRange, BlockRange)> {
    let mut out = Vec::new();
    for x in a {
        for y in b {
            if x.overlaps_with(y) {
                out.push((*x, *y));
            }
        }
    }
    out
}

/// A delta plus what the sender says it touched.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct DeltaPacket {
    /// The sender's revision *after* these edits.
    pub revision: u64,
    /// The sender's revision these edits were computed against.
    pub base_revision: u64,
    /// Which blocks the sender believes it changed.
    pub blocks: Vec<BlockRange>,
    pub bytes: Vec<u8>,
}

/// What a peer did that this document also did, since a shared base.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct ConflictRecord {
    pub peer: String,
    /// The peer's revision and the one it based its work on.
    pub their_revision: u64,
    pub their_base: u64,
    /// This document's revision at the time of the import.
    pub my_revision: u64,
    /// Ranges both sides touched.
    pub their_blocks: Vec<BlockRange>,
    pub my_blocks: Vec<BlockRange>,
    /// The specific overlaps, not just the two sets.
    pub overlaps: Vec<(BlockRange, BlockRange)>,
}

impl ConflictRecord {
    /// A sentence an operator or a worker can act on.
    pub fn explain(&self) -> String {
        let pairs = self
            .overlaps
            .iter()
            .map(|(t, m)| format!("{}..{} vs {}..{}", t.start, t.end, m.start, m.end))
            .collect::<Vec<_>>()
            .join(", ");
        format!(
            "{} edited the same block{} as this document since revision {} (now at \
             revision {}): {}. The edits were merged; re-check the acceptance \
             criteria for both sides before keeping the result.",
            self.peer,
            if self.overlaps.len() == 1 { "" } else { "s" },
            self.their_base,
            self.my_revision,
            pairs
        )
    }
}

/// The result of importing a packet.
#[derive(Debug, Clone, PartialEq)]
pub enum ImportOutcome {
    /// Merged, and nothing both sides had touched.
    Clean,
    /// Merged, but both sides had touched the same block.
    Conflicted(ConflictRecord),
}

impl ImportOutcome {
    pub fn conflict(&self) -> Option<&ConflictRecord> {
        match self {
            ImportOutcome::Clean => None,
            ImportOutcome::Conflicted(c) => Some(c),
        }
    }

    pub fn is_clean(&self) -> bool {
        matches!(self, ImportOutcome::Clean)
    }
}

/// Tracks which blocks this document changed, per revision.
///
/// The point of storing *ranges per revision* rather than one merged set is that
/// "changed since revision R" has to be answerable: an incoming packet says it
/// was based on R, and the receiver needs to know what it did after R. A single
/// running set cannot answer that.
#[derive(Debug, Clone, Default)]
pub struct Footprint {
    /// `history[i]` is what changed at revision `base_revision + i + 1`.
    history: Vec<Vec<BlockRange>>,
    /// The absolute revision of `history[0]`.
    ///
    /// Tracked separately because `revision()` must never go *backwards*. When
    /// this was `history.len()`, trimming history silently rewound the document
    /// revision -- and a peer based on the new, lower number would be told its
    /// work predates everything we have done, which is the one answer that must
    /// never be given by accident.
    base_revision: u64,
}

impl Footprint {
    pub fn new() -> Footprint {
        Footprint::default()
    }

    /// Current revision. Starts at 0: an untouched document is at revision 0, so
    /// a peer based on 0 has seen everything.
    pub fn revision(&self) -> u64 {
        self.base_revision + self.history.len() as u64
    }

    /// Record what changed at the next revision.
    pub fn record(&mut self, blocks: Vec<BlockRange>) -> u64 {
        self.history.push(blocks);
        self.revision()
    }

    /// Everything changed in revisions after `base`, coalesced.
    ///
    /// Three cases, and the middle one is the one that is easy to get wrong:
    ///
    /// - `base == revision()`: the peer is exactly up to date and has seen
    ///   everything. **Nothing** is "since", and reporting otherwise would
    ///   manufacture a conflict on every well-behaved exchange.
    /// - `base > revision()`: the peer is ahead of us -- our history is
    ///   truncated, or we are a fresh replica. We cannot prove its work is
    ///   disjoint from ours, so we return **everything we know**. False positives
    ///   are recoverable; a missed collision is not.
    /// - `base < our retained window`: the same uncertainty from the other side,
    ///   handled the same way.
    pub fn since(&self, base: u64) -> Vec<BlockRange> {
        if base == self.revision() {
            return Vec::new();
        }
        let from = if base > self.revision() {
            0
        } else {
            // saturating_sub: a peer below the retained window lands on 0
            // (everything retained) rather than wrapping around.
            base.saturating_sub(self.base_revision) as usize
        };
        let mut out: Vec<BlockRange> = Vec::new();
        for revs in &self.history[from.min(self.history.len())..] {
            for r in revs {
                match out.iter_mut().find(|e: &&mut BlockRange| e.overlaps(r)) {
                    Some(e) => *e = e.union(r),
                    None => out.push(*r),
                }
            }
        }
        out
    }

    /// Drop history below `keep_from`, to bound memory.
    ///
    /// `revision()` is unaffected -- `base_revision` absorbs the trim. A peer
    /// based below the retained window gets the fail-closed answer from
    /// [`Footprint::since`], which is the intended degradation.
    pub fn truncate_below(&mut self, keep_from: u64) {
        // Keep revisions >= keep_from. `history[i]` is revision
        // `base_revision + i + 1`, so the first kept index is
        // `keep_from - base_revision - 1`. The `- 1` matters: dropping
        // `keep_from - base_revision` entries discarded `keep_from` itself,
        // silently losing the very revision the caller asked to keep.
        let drop_n = keep_from
            .saturating_sub(self.base_revision + 1)
            .min(self.history.len() as u64) as usize;
        if drop_n > 0 {
            self.history.drain(..drop_n);
            self.base_revision += drop_n as u64;
        }
    }
}

/// Tracks revisions and footprints for one collaborating document.
#[derive(Debug, Clone, Default)]
pub struct ConflictTracker {
    peer: String,
    mine: Footprint,
    /// What each peer last told us it touched, keyed by its revision.
    theirs: std::collections::BTreeMap<u64, Vec<BlockRange>>,
}

impl ConflictTracker {
    pub fn new(peer: impl Into<String>) -> ConflictTracker {
        ConflictTracker {
            peer: peer.into(),
            mine: Footprint::new(),
            theirs: std::collections::BTreeMap::new(),
        }
    }

    pub fn revision(&self) -> u64 {
        self.mine.revision()
    }

    /// Record a local edit and package it for sending.
    pub fn local_edit(&mut self, blocks: Vec<BlockRange>, bytes: Vec<u8>) -> DeltaPacket {
        let base = self.mine.revision();
        let revision = self.mine.record(blocks.clone());
        self.theirs.insert(revision, blocks.clone());
        DeltaPacket {
            revision,
            base_revision: base,
            blocks,
            bytes,
        }
    }

    /// Classify an incoming packet against what we have done since its base.
    ///
    /// Pure with respect to the document: it does not import. Callers apply the
    /// bytes however they like; this says whether the two sides overlapped.
    pub fn classify(&self, packet: &DeltaPacket) -> ImportOutcome {
        let mine_since = self.mine.since(packet.base_revision);
        if !footprints_overlap(&packet.blocks, &mine_since) {
            return ImportOutcome::Clean;
        }
        let overlaps = overlapping_pairs(&packet.blocks, &mine_since);
        ImportOutcome::Conflicted(ConflictRecord {
            peer: self.peer.clone(),
            their_revision: packet.revision,
            their_base: packet.base_revision,
            my_revision: self.mine.revision(),
            their_blocks: packet.blocks.clone(),
            my_blocks: mine_since,
            overlaps,
        })
    }
}

#[cfg(test)]
mod conflict_tests {
    use super::*;

    fn rng(start: usize, end: usize) -> BlockRange {
        BlockRange::new(start, end)
    }

    // ---- range arithmetic --------------------------------------------------

    #[test]
    fn overlapping_ranges_are_detected() {
        assert!(rng(0, 10).overlaps(&rng(5, 15)));
        assert!(rng(5, 15).overlaps(&rng(0, 10)));
        assert!(rng(0, 10).overlaps(&rng(0, 10)));
    }

    #[test]
    fn adjacent_ranges_do_not_overlap() {
        assert!(!rng(0, 10).overlaps(&rng(10, 20)));
        assert!(!rng(10, 20).overlaps(&rng(0, 10)));
    }

    #[test]
    fn an_empty_range_never_overlaps_another_empty_range() {
        // Two insertions at the same point are not a conflict with each other.
        assert!(!rng(5, 5).overlaps(&rng(5, 5)));
        assert!(rng(5, 5).is_empty());
    }

    #[test]
    fn an_insertion_at_the_edge_of_someone_elses_edit_is_a_conflict() {
        // Disjoint byte ranges, but appending inside a derivation being rewritten
        // is a conflict in practice.
        assert!(!rng(5, 5).overlaps(&rng(0, 10)));
        assert!(rng(5, 5).overlaps_with(&rng(0, 10)));
        assert!(rng(0, 10).overlaps_with(&rng(5, 5)));
        // At the very boundary.
        assert!(rng(0, 10).overlaps_with(&rng(10, 10)));
        assert!(!rng(0, 10).overlaps_with(&rng(11, 11)));
    }

    #[test]
    fn overlapping_pairs_name_both_sides() {
        let pairs = overlapping_pairs(&[rng(0, 10), rng(100, 110)], &[rng(5, 15)]);
        assert_eq!(pairs, vec![(rng(0, 10), rng(5, 15))]);
    }

    // ---- the footprint history ---------------------------------------------

    #[test]
    fn an_untouched_document_is_at_revision_zero() {
        let f = Footprint::new();
        assert_eq!(f.revision(), 0);
        // A peer based on 0 has seen everything, so nothing is "since".
        assert!(f.since(0).is_empty());
    }

    #[test]
    fn since_excludes_the_base_revision_itself() {
        let mut f = Footprint::new();
        f.record(vec![rng(0, 10)]); // revision 1
        f.record(vec![rng(50, 60)]); // revision 2
        // "Since revision 2" means after it, and nothing came after.
        assert!(f.since(2).is_empty());
        assert_eq!(f.since(1), vec![rng(50, 60)]);
        assert_eq!(f.since(0).len(), 2);
    }

    #[test]
    fn since_merges_touching_ranges() {
        let mut f = Footprint::new();
        f.record(vec![rng(0, 10)]);
        f.record(vec![rng(8, 20)]);
        let since = f.since(0);
        assert_eq!(since.len(), 1, "touching ranges should coalesce: {since:?}");
        assert_eq!(since[0], rng(0, 20));
    }

    #[test]
    fn an_unknown_base_fails_closed() {
        // A peer claiming to be further along than we are means our history is
        // truncated or we are a fresh replica. We cannot prove disjointness, so
        // we must not claim it.
        let mut f = Footprint::new();
        f.record(vec![rng(0, 10)]);
        let since = f.since(9999);
        assert_eq!(since, vec![rng(0, 10)], "everything we know is returned");
    }

    #[test]
    fn truncating_history_degrades_to_the_fail_closed_answer() {
        let mut f = Footprint::new();
        f.record(vec![rng(0, 10)]); // rev 1
        f.record(vec![rng(20, 30)]); // rev 2
        f.truncate_below(2);
        assert_eq!(
            f.revision(),
            2,
            "revision() must not rewind when history is trimmed"
        );
        // A peer based below the window gets everything retained.
        assert_eq!(f.since(0), vec![rng(20, 30)]);
    }

    // ---- classification ----------------------------------------------------

    fn packet(base: u64, revision: u64, blocks: Vec<BlockRange>) -> DeltaPacket {
        DeltaPacket {
            revision,
            base_revision: base,
            blocks,
            bytes: vec![],
        }
    }

    #[test]
    fn a_peer_working_on_disjoint_blocks_imports_clean() {
        let mut t = ConflictTracker::new("w2");
        t.local_edit(vec![rng(0, 10)], vec![]);
        let p = packet(0, 2, vec![rng(100, 110)]);
        assert!(t.classify(&p).is_clean());
    }

    #[test]
    fn a_peer_touching_the_same_block_is_reported() {
        let mut t = ConflictTracker::new("w2");
        t.local_edit(vec![rng(0, 10)], vec![]);
        let p = packet(0, 2, vec![rng(5, 15)]);
        let outcome = t.classify(&p);
        let c = outcome.conflict().expect("must be reported");
        assert_eq!(c.peer, "w2");
        assert_eq!(c.their_base, 0);
        assert_eq!(c.overlaps, vec![(rng(5, 15), rng(0, 10))]);
    }

    #[test]
    fn work_done_before_the_peers_base_is_not_a_conflict() {
        // The peer is up to date with us, so our old edits cannot collide.
        let mut t = ConflictTracker::new("w2");
        t.local_edit(vec![rng(0, 10)], vec![]);
        let p = packet(1, 2, vec![rng(0, 10)]);
        assert!(
            t.classify(&p).is_clean(),
            "the peer already had this block"
        );
    }

    #[test]
    fn an_undeclared_footprint_is_treated_as_clean_but_says_nothing() {
        // A peer that sends bare bytes has told us nothing, so nothing can be
        // reported. That is a limitation of the wire format, not a clean bill of
        // health -- documented rather than papered over.
        let mut t = ConflictTracker::new("legacy");
        t.local_edit(vec![rng(0, 10)], vec![]);
        let p = packet(0, 1, vec![]);
        assert!(t.classify(&p).is_clean());
    }

    #[test]
    fn a_conflict_record_explains_itself() {
        let mut t = ConflictTracker::new("w2");
        t.local_edit(vec![rng(0, 10)], vec![]);
        let c = t.classify(&packet(0, 2, vec![rng(5, 15)])).conflict().unwrap().clone();
        let s = c.explain();
        assert!(s.contains("w2"), "{s}");
        assert!(s.contains("re-check"), "it should say what to do: {s}");
        assert!(s.contains("merged"), "it should be clear the edits were kept: {s}");
        assert!(s.len() > 80, "too terse to act on: {s}");
    }

    // ---- against the real CRDT --------------------------------------------

    #[test]
    fn two_agents_editing_one_block_converge_and_the_collision_is_surfaced() {
        // The point of the whole item: the CRDT still converges (no divergence),
        // *and* the collision is reported instead of being silently merged.
        let mut doc_a = MathDoc::new();
        let mut doc_b = MathDoc::new();
        doc_a.insert(0, "shared block");
        doc_b.import_delta(&doc_a.export_delta()).unwrap();

        let mut ta = ConflictTracker::new("doc_b");
        let mut tb = ConflictTracker::new("doc_a");

        // Both rewrite the same block.
        let len = doc_a.text().len();
        doc_a.insert(len, " A edit");
        let pa = ta.local_edit(vec![rng(0, doc_a.text().len())], doc_a.export_delta());

        doc_b.insert(doc_b.text().len(), " B edit");
        let pb = tb.local_edit(vec![rng(0, doc_b.text().len())], doc_b.export_delta());

        // Each side classifies the other's work *before* applying it.
        let seen_by_a = ta.classify(&pb).conflict().cloned();
        let seen_by_b = tb.classify(&pa).conflict().cloned();
        assert!(seen_by_a.is_some(), "A must see the collision");
        assert!(seen_by_b.is_some(), "B must see the collision");

        // Applied anyway: refusing would leave them diverged.
        doc_a.import_delta(&pb.bytes).unwrap();
        doc_b.import_delta(&pa.bytes).unwrap();

        assert_eq!(
            doc_a.text(),
            doc_b.text(),
            "convergence is not abandoned just because we reported a conflict"
        );
    }

    #[test]
    fn two_agents_editing_different_blocks_converge_without_a_conflict() {
        let mut doc_a = MathDoc::new();
        let mut doc_b = MathDoc::new();
        doc_a.insert(0, "AAAA\nBBBB");
        doc_b.import_delta(&doc_a.export_delta()).unwrap();

        let mut ta = ConflictTracker::new("doc_b");
        let mut tb = ConflictTracker::new("doc_a");

        // A edits the first line, B the second.
        let la = doc_a.text().len();
        doc_a.insert(la, " more A");
        let pa = ta.local_edit(vec![rng(0, 4)], doc_a.export_delta());

        doc_b.insert(0, "more B ");
        let pb = tb.local_edit(vec![rng(5, 9)], doc_b.export_delta());

        assert!(
            ta.classify(&pb).is_clean(),
            "disjoint blocks must not be reported"
        );
        assert!(tb.classify(&pa).is_clean());

        doc_a.import_delta(&pb.bytes).unwrap();
        doc_b.import_delta(&pa.bytes).unwrap();
        assert_eq!(doc_a.text(), doc_b.text());
    }
}
