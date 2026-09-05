//! Content-keyed raster memos — the editor's extension of Typst's own
//! memoization.
//!
//! Typst's cache is comemo memoization, activated per compile pass
//! inside `typst::compile` and discarded when the pass ends; it never
//! spans our per-frame draws. The memos here are the derived-state
//! extension point: a raster is kept while its content hash and its
//! layout width are unchanged, so an unchanged overlay is a pure blit
//! and a fresh Typst compile happens only when its content actually
//! changed. World construction (fonts/library) is separately shared
//! process-wide in [`crate::world`].
//!
//! F3: entries are keyed by (site, width) — resizing back to a width
//! that has been seen is a hit, not a recompile — and the store keeps
//! a budget of raster bytes with LRU eviction, so width churn cannot
//! grow memory without bound. Hits/compiles/evictions are accounted
//! for the report emitted on overlay close.

use std::collections::{HashMap, VecDeque};

/// A doc block's cached layout plus the content fingerprint it was
/// built from (F2): the editor keeps a block's raster while its
/// content key is unchanged — an edit or a kernel-results change only
/// re-lays out the blocks whose rendered output could actually
/// differ, instead of clearing the whole cache. Deref to
/// [`crate::render::DocLayout`] keeps the draw and hit-test sites
/// unchanged.
pub struct BlockLayout {
    /// Fingerprint of everything [`crate::render::layout_block`]
    /// consumes for this block (doc slice, reveal ranges,
    /// per-block annotations/errors, width).
    pub key: u64,
    /// The laid-out page.
    pub layout: crate::render::DocLayout,
}

impl std::ops::Deref for BlockLayout {
    type Target = crate::render::DocLayout;

    fn deref(&self) -> &Self::Target {
        &self.layout
    }
}

/// A block's cached output-region raster plus the content
/// fingerprint it was built from (F3b): the same content-keyed
/// treatment as [`BlockLayout`] for the result regions — a kernel
/// result landing in one block only re-renders that block's region.
/// Derefs to [`imaging::RgbaImage`] to keep the draw site unchanged.
pub struct RegionEntry {
    /// Fingerprint of the region's content (block outputs + stale
    /// flag + width).
    pub key: u64,
    /// The rasterized region.
    pub image: imaging::RgbaImage,
}

impl std::ops::Deref for RegionEntry {
    type Target = imaging::RgbaImage;

    fn deref(&self) -> &Self::Target {
        &self.image
    }
}

/// A cached raster plus the (content, width) fingerprint it was built
/// from.
pub struct RasterMemo {
    /// Window width (px) the raster was laid out at (0 for rasters
    /// whose render width is fixed, e.g. the doc preview).
    pub width: u32,
    /// Content hash (content + width) the raster was built from.
    pub key: u64,
    /// The rasterized image.
    pub image: imaging::RgbaImage,
}

/// Default memory budget for the whole store (sum of raster bytes).
pub const DEFAULT_CAP_BYTES: u64 = 128 * 1024 * 1024;

/// The content-keyed raster store.
pub struct MemoStore {
    memos: HashMap<(&'static str, u32), RasterMemo>,
    /// Recency queue of (site, width) keys; back = most recently used.
    lru: VecDeque<(&'static str, u32)>,
    /// Sum of the stored rasters' byte lengths.
    total_bytes: u64,
    /// Eviction budget; insertion evicts oldest-used entries above it.
    cap_bytes: u64,
    /// Accounting (F4): hits vs fresh compiles vs evictions since the
    /// last [`MemoStore::take_accounting`].
    pub hits: u64,
    pub compiles: u64,
    pub evictions: u64,
}

impl MemoStore {
    /// An empty store under [`DEFAULT_CAP_BYTES`].
    pub fn new() -> Self {
        Self::with_cap(DEFAULT_CAP_BYTES)
    }

    /// An empty store under `cap_bytes` of raster budget.
    pub fn with_cap(cap_bytes: u64) -> Self {
        Self {
            memos: HashMap::new(),
            lru: VecDeque::new(),
            total_bytes: 0,
            cap_bytes,
            hits: 0,
            compiles: 0,
            evictions: 0,
        }
    }

    /// Record a hit and return the raster when a memo for `site` at
    /// `width` exists with content key `key`; otherwise `None` (the
    /// caller compiles and [`MemoStore::insert`]s). A hit also bumps
    /// the entry to most-recently-used.
    pub fn get(&mut self, site: &'static str, width: u32, key: u64) -> Option<&imaging::RgbaImage> {
        if !self.memos.get(&(site, width)).is_some_and(|m| m.key == key) {
            return None;
        }
        self.hits += 1;
        self.touch(site, width);
        self.memos.get(&(site, width)).map(|m| &m.image)
    }

    /// Look up a raster without accounting (draw sites after the
    /// pre-pass has already hit/missed).
    pub fn image(&self, site: &'static str, width: u32) -> Option<&imaging::RgbaImage> {
        self.memos.get(&(site, width)).map(|m| &m.image)
    }

    /// The height of the stored raster for `site` at `width`, if any
    /// (scroll clamping).
    pub fn image_height(&self, site: &'static str, width: u32) -> Option<usize> {
        self.memos
            .get(&(site, width))
            .map(|m| m.image.height as usize)
    }

    /// Store a freshly compiled raster for `site` at `width`, evicting
    /// least-recently-used entries while the store exceeds its byte
    /// budget. Records one compile.
    pub fn insert(&mut self, site: &'static str, width: u32, key: u64, image: imaging::RgbaImage) {
        self.compiles += 1;
        if let Some(old) = self.memos.get(&(site, width)) {
            self.total_bytes = self.total_bytes.saturating_sub(old.image.data.len() as u64);
        }
        self.touch(site, width);
        let bytes = image.data.len() as u64;
        self.total_bytes += bytes;
        self.memos
            .insert((site, width), RasterMemo { width, key, image });
        // Evict oldest-used until under budget. Never evict the entry
        // just inserted (a single raster larger than the whole budget
        // is kept: dropping it would make every frame a compile).
        while self.total_bytes > self.cap_bytes && self.lru.len() > 1 {
            if let Some((esite, ewidth)) = self.lru.pop_front()
                && let Some(old) = self.memos.remove(&(esite, ewidth))
            {
                self.total_bytes = self.total_bytes.saturating_sub(old.image.data.len() as u64);
                self.evictions += 1;
            }
        }
    }

    /// Drop the memo for `site` at `width` (a render failed — the
    /// overlay then draws nothing, as before).
    pub fn remove(&mut self, site: &'static str, width: u32) {
        if let Some(old) = self.memos.remove(&(site, width)) {
            self.total_bytes = self.total_bytes.saturating_sub(old.image.data.len() as u64);
        }
        self.lru.retain(|k| *k != (site, width));
    }

    /// Mark (site, width) most-recently-used (moves it to the back of
    /// the recency queue).
    fn touch(&mut self, site: &'static str, width: u32) {
        self.lru.retain(|k| *k != (site, width));
        self.lru.push_back((site, width));
    }

    /// Current raster bytes under management (tests).
    #[cfg(test)]
    fn total_bytes(&self) -> u64 {
        self.total_bytes
    }

    /// Number of distinct (site, width) entries (tests).
    #[cfg(test)]
    fn len(&self) -> usize {
        self.memos.len()
    }

    /// Take the accounting counters since the last call (F4 report on
    /// overlay close).
    pub fn take_accounting(&mut self) -> (u64, u64, u64) {
        let a = (self.hits, self.compiles, self.evictions);
        self.hits = 0;
        self.compiles = 0;
        self.evictions = 0;
        a
    }
}

impl Default for MemoStore {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rgba(w: u32, h: u32) -> imaging::RgbaImage {
        imaging::RgbaImage::new(w, h)
    }

    #[test]
    fn hit_requires_site_width_and_content_key() {
        let mut s = MemoStore::new();
        s.insert("help", 100, 7, rgba(10, 10));
        assert!(s.get("help", 100, 7).is_some(), "full hit");
        // Same site, different width: miss (resize recompiles — the
        // caller then inserts a new entry, not a replacement).
        assert!(s.get("help", 200, 7).is_none());
        // Same site+width, stale content key: miss.
        assert!(s.get("help", 100, 8).is_none());
        // Different site: miss.
        assert!(s.get("media_menu", 100, 7).is_none());
        let (hits, compiles, _) = s.take_accounting();
        assert_eq!(
            (hits, compiles),
            (1, 1),
            "one hit and the one setup compile counted"
        );
    }

    #[test]
    fn multiple_widths_are_kept_side_by_side() {
        let mut s = MemoStore::new();
        s.insert("kernel_menu", 100, 1, rgba(5, 5));
        s.insert("kernel_menu", 200, 2, rgba(6, 6));
        s.insert("kernel_menu", 300, 3, rgba(7, 7));
        assert_eq!(s.len(), 3, "one entry per width");
        // Returning to an earlier width is a hit, not a recompile.
        assert!(s.get("kernel_menu", 100, 1).is_some());
        assert!(s.get("kernel_menu", 300, 3).is_some());
        let (hits, compiles, _) = s.take_accounting();
        assert_eq!((hits, compiles), (2, 3), "two hits + three setup compiles");
    }

    #[test]
    fn byte_budget_evicts_least_recently_used() {
        // 4×4 rgba = 64 bytes each; budget holds 3.
        let mut s = MemoStore::with_cap(64 * 3);
        s.insert("a", 1, 1, rgba(4, 4));
        s.insert("b", 1, 1, rgba(4, 4));
        s.insert("c", 1, 1, rgba(4, 4));
        assert_eq!(s.len(), 3);
        // Touching `a` makes it most-recently-used; the next insert
        // must evict `b` (oldest) and only `b`: 4×64 = 256 > 192
        // budget, one eviction brings it to exactly 192.
        s.get("a", 1, 1);
        s.insert("d", 1, 1, rgba(4, 4));
        assert_eq!(s.len(), 3, "budget holds exactly 3");
        assert!(s.image("a", 1).is_some(), "recently used survives");
        assert!(s.image("c", 1).is_some(), "untouched survives");
        assert!(s.image("d", 1).is_some(), "newest survives");
        assert!(s.image("b", 1).is_none(), "oldest evicted");
        assert!(s.total_bytes() <= 64 * 3);
        let (_, _, evictions) = s.take_accounting();
        assert_eq!(evictions, 1);
    }

    #[test]
    fn replacement_does_not_double_count_bytes() {
        let mut s = MemoStore::new();
        s.insert("a", 1, 1, rgba(4, 4)); // 64 bytes
        assert_eq!(s.total_bytes(), 64);
        s.insert("a", 1, 2, rgba(8, 8)); // 256 bytes replaces 64
        assert_eq!(s.total_bytes(), 256, "old bytes freed on replace");
        assert_eq!(s.len(), 1);
    }

    #[test]
    fn remove_frees_bytes_and_lru_slot() {
        let mut s = MemoStore::new();
        s.insert("a", 1, 1, rgba(4, 4));
        s.remove("a", 1);
        assert_eq!(s.total_bytes(), 0);
        assert!(s.get("a", 1, 1).is_none());
        let (hits, _, _) = s.take_accounting();
        assert_eq!(hits, 0, "a post-remove get is a miss, not a hit");
    }
}
