//! Which tiles are on screen, and a memory-bounded cache for the ones already rendered.

use std::collections::HashMap;
use std::ops::Range;

use pdf_engine::{TILE_SIZE, TileKey};

/// Tile columns and rows of a page that overlap the viewport.
///
/// `page_origin` is the page's top-left corner relative to the viewport's top-left corner,
/// and all values are in device pixels.
pub fn visible_tiles(
    page_px: (u32, u32),
    page_origin: (f32, f32),
    viewport_px: (f32, f32),
) -> (Range<u32>, Range<u32>) {
    let axis = |page_len: u32, origin: f32, view_len: f32| {
        // The part of the page (in page pixels) that falls inside the viewport.
        let start = (-origin).max(0.0);
        let end = (view_len - origin).min(page_len as f32);
        if end <= start {
            return 0..0;
        }
        let tile = TILE_SIZE as f32;
        (start / tile).floor() as u32..(end / tile).ceil() as u32
    };
    (
        axis(page_px.0, page_origin.0, viewport_px.0),
        axis(page_px.1, page_origin.1, viewport_px.1),
    )
}

/// Rendered tiles, evicting the least recently drawn ones when over the memory budget.
/// Tiles drawn in the current frame are never evicted, so the screen cannot lose tiles it
/// is showing even if the budget is too small.
pub struct TileCache<V> {
    entries: HashMap<TileKey, Entry<V>>,
    bytes: usize,
    budget: usize,
    frame: u64,
}

struct Entry<V> {
    value: V,
    bytes: usize,
    last_used: u64,
}

impl<V> TileCache<V> {
    pub fn new(budget_bytes: usize) -> Self {
        TileCache {
            entries: HashMap::new(),
            bytes: 0,
            budget: budget_bytes,
            frame: 0,
        }
    }

    /// Call once per frame before any `get`.
    pub fn begin_frame(&mut self) {
        self.frame += 1;
    }

    /// Returns the tile and marks it as used in this frame.
    pub fn get(&mut self, key: &TileKey) -> Option<&V> {
        let frame = self.frame;
        self.entries.get_mut(key).map(|e| {
            e.last_used = frame;
            &e.value
        })
    }

    /// Returns the tile without marking it as used.
    pub fn peek(&self, key: &TileKey) -> Option<&V> {
        self.entries.get(key).map(|e| &e.value)
    }

    pub fn contains(&self, key: &TileKey) -> bool {
        self.entries.contains_key(key)
    }

    pub fn insert(&mut self, key: TileKey, value: V, bytes: usize) {
        if let Some(old) = self.entries.insert(
            key,
            Entry {
                value,
                bytes,
                last_used: self.frame,
            },
        ) {
            self.bytes -= old.bytes;
        }
        self.bytes += bytes;
        self.evict();
    }

    pub fn clear(&mut self) {
        self.entries.clear();
        self.bytes = 0;
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn bytes(&self) -> usize {
        self.bytes
    }

    fn evict(&mut self) {
        while self.bytes > self.budget {
            let oldest = self
                .entries
                .iter()
                .filter(|(_, e)| e.last_used < self.frame)
                .min_by_key(|(_, e)| e.last_used)
                .map(|(k, _)| *k);
            let Some(key) = oldest else { break };
            let entry = self.entries.remove(&key).expect("key was just found");
            self.bytes -= entry.bytes;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pdf_engine::{DocId, Scale};

    fn key(tx: u32) -> TileKey {
        TileKey {
            doc: DocId(1),
            page: 0,
            scale: Scale::from_px_per_pt(1.0),
            tx,
            ty: 0,
        }
    }

    #[test]
    fn page_fully_inside_the_viewport() {
        let (cols, rows) = visible_tiles((1190, 1684), (10.0, 10.0), (2000.0, 2000.0));
        assert_eq!((cols, rows), (0..3, 0..4));
    }

    #[test]
    fn page_scrolled_partly_out_of_view() {
        // Top 1100 px of the page are above the viewport, which is 600 px tall.
        let (cols, rows) = visible_tiles((1190, 1684), (0.0, -1100.0), (1190.0, 600.0));
        assert_eq!(cols, 0..3);
        assert_eq!(rows, 2..4);
    }

    #[test]
    fn page_below_or_above_the_viewport_has_no_tiles() {
        let (_, rows) = visible_tiles((1190, 1684), (0.0, 700.0), (1190.0, 600.0));
        assert!(rows.is_empty());
        let (_, rows) = visible_tiles((1190, 1684), (0.0, -2000.0), (1190.0, 600.0));
        assert!(rows.is_empty());
    }

    #[test]
    fn exact_tile_boundaries_do_not_add_extra_tiles() {
        let (_, rows) = visible_tiles((512, 2048), (0.0, -512.0), (512.0, 512.0));
        assert_eq!(rows, 1..2);
    }

    #[test]
    fn evicts_least_recently_drawn_first() {
        let mut cache = TileCache::new(3);
        cache.begin_frame();
        cache.insert(key(0), "a", 1);
        cache.insert(key(1), "b", 1);
        cache.begin_frame();
        cache.insert(key(2), "c", 1);
        cache.begin_frame();
        cache.get(&key(0)); // tile 0 is still on screen
        cache.insert(key(3), "d", 1);
        assert!(cache.contains(&key(0)));
        assert!(!cache.contains(&key(1)), "oldest unused tile is evicted");
        assert_eq!((cache.len(), cache.bytes()), (3, 3));
    }

    #[test]
    fn never_evicts_tiles_drawn_this_frame() {
        let mut cache = TileCache::new(1);
        cache.begin_frame();
        cache.insert(key(0), (), 1);
        cache.insert(key(1), (), 1);
        assert_eq!(cache.len(), 2, "both are in use, so the budget is exceeded");
        cache.begin_frame();
        cache.get(&key(1));
        cache.insert(key(2), (), 1);
        assert!(!cache.contains(&key(0)));
    }

    #[test]
    fn replacing_a_tile_keeps_the_byte_count_right() {
        let mut cache = TileCache::new(100);
        cache.insert(key(0), (), 10);
        cache.insert(key(0), (), 30);
        assert_eq!(cache.bytes(), 30);
        cache.clear();
        assert_eq!(cache.bytes(), 0);
    }
}
