//! Keeps the page tiles on screen up to date: asks the engine for missing tiles, uploads
//! finished ones to the GPU (a few per frame), and keeps them in a memory-bounded cache.

use std::collections::{HashSet, VecDeque};

use eframe::egui::{self, Color32, ColorImage, TextureHandle, TextureOptions};
use pdf_engine::{Engine, TILE_SIZE, Tile, TileKey, TileRequest, TileResult};
use pdf_view::TileCache;

/// GPU memory for page tiles. A 512×512 tile takes 1 MB.
const MEMORY_BUDGET: usize = 300 * 1024 * 1024;

/// GPU upload budget per frame. Uploads run at the end of the frame, so the limit is in
/// bytes. 12 MB is 12 full tiles: after a zoom settles, a 1080p view (about 12 tiles) turns
/// sharp in one frame. perf-runner's `ui_cpu_p99_ms` checks that frames stay in budget.
const UPLOAD_BYTES_PER_FRAME: usize = 12 * 1024 * 1024;

/// Scale of the page preview shown until the sharp tiles arrive (about 22 DPI): cheap
/// enough to render for every page the moment it scrolls into view.
const PREVIEW_PX_PER_PT: f32 = 0.3;

pub struct TileManager {
    cache: TileCache<TextureHandle>,
    /// Finished tiles waiting for their turn to be uploaded.
    ready: VecDeque<TileResult>,
    ready_keys: HashSet<TileKey>,
    failed: HashSet<TileKey>,
    wanted: Vec<TileRequest>,
    last_wanted: Vec<TileKey>,
    generation: u64,
    stats: TileStats,
}

/// Counters for performance reports.
#[derive(Clone, Debug, Default)]
pub struct TileStats {
    /// Tiles received from the engine.
    pub rendered: u64,
    /// Engine render time of the most recent tiles, in milliseconds.
    pub render_ms: VecDeque<f32>,
    pub peak_cache_bytes: usize,
    /// Most finished tiles ever waiting for upload at once, and their bytes.
    pub peak_ready: usize,
    pub peak_ready_bytes: usize,
    /// Finished tiles thrown away because the view had moved on.
    pub discarded: u64,
}

/// How many recent render times [`TileStats`] keeps.
const RENDER_TIMES_KEPT: usize = 10_000;

impl TileManager {
    pub fn new() -> Self {
        TileManager {
            cache: TileCache::new(MEMORY_BUDGET),
            ready: VecDeque::new(),
            ready_keys: HashSet::new(),
            failed: HashSet::new(),
            wanted: Vec::new(),
            last_wanted: Vec::new(),
            generation: 0,
            stats: TileStats::default(),
        }
    }

    /// Collects finished tiles and uploads up to [`UPLOAD_BYTES_PER_FRAME`] of them.
    pub fn begin_frame(&mut self, engine: &Engine, ctx: &egui::Context) {
        self.cache.begin_frame();
        for result in engine.results().try_iter() {
            if let Ok(tile) = &result.tile {
                self.stats.rendered += 1;
                if self.stats.render_ms.len() == RENDER_TIMES_KEPT {
                    self.stats.render_ms.pop_front();
                }
                self.stats
                    .render_ms
                    .push_back(tile.render_time.as_secs_f32() * 1000.0);
            }
            self.ready_keys.insert(result.key);
            self.ready.push_back(result);
        }
        // Tiles the view no longer asks for (it scrolled or zoomed on while they rendered)
        // would only cost upload time and memory.
        let before = self.ready.len();
        let wanted = &self.last_wanted;
        let ready_keys = &mut self.ready_keys;
        self.ready.retain(|r| {
            let keep = wanted.binary_search(&r.key).is_ok();
            if !keep {
                ready_keys.remove(&r.key);
            }
            keep
        });
        self.stats.discarded += (before - self.ready.len()) as u64;

        self.stats.peak_ready = self.stats.peak_ready.max(self.ready.len());
        let ready_bytes: usize = self
            .ready
            .iter()
            .map(|r| r.tile.as_ref().map_or(0, |t| t.rgba.len()))
            .sum();
        self.stats.peak_ready_bytes = self.stats.peak_ready_bytes.max(ready_bytes);

        let mut uploaded = 0;
        while uploaded < UPLOAD_BYTES_PER_FRAME {
            let Some(result) = self.ready.pop_front() else {
                break;
            };
            self.ready_keys.remove(&result.key);
            match result.tile {
                Ok(tile) => {
                    let bytes = tile.rgba.len();
                    let texture =
                        ctx.load_texture("page tile", color_image(tile), TextureOptions::LINEAR);
                    self.cache.insert(result.key, texture, bytes);
                    self.stats.peak_cache_bytes =
                        self.stats.peak_cache_bytes.max(self.cache.bytes());
                    uploaded += bytes;
                }
                Err(_) => {
                    self.failed.insert(result.key);
                }
            }
        }
        self.wanted.clear();
    }

    /// Returns the tile's texture if it is ready; otherwise asks for it with the given
    /// priority (lower renders sooner).
    pub fn get(&mut self, key: TileKey, priority: u32) -> Option<TextureHandle> {
        if let Some(texture) = self.cache.get(&key) {
            return Some(texture.clone());
        }
        if !self.failed.contains(&key) && !self.ready_keys.contains(&key) {
            self.wanted.push(TileRequest {
                key,
                generation: 0,
                priority,
            });
        }
        None
    }

    /// Returns the tile's texture if it is already cached, without requesting it.
    pub fn peek(&mut self, key: TileKey) -> Option<TextureHandle> {
        self.cache.get(&key).cloned()
    }

    /// Sends this frame's requests. When the set of wanted tiles changed (the view
    /// scrolled), the engine drops queued tiles nobody is waiting for any more.
    pub fn end_frame(&mut self, engine: &Engine) {
        let mut keys: Vec<TileKey> = self.wanted.iter().map(|r| r.key).collect();
        keys.sort();
        if keys == self.last_wanted {
            return;
        }
        self.generation += 1;
        engine.set_generation(self.generation);
        for request in &self.wanted {
            engine.request_tile(TileRequest {
                generation: self.generation,
                ..*request
            });
        }
        self.last_wanted = keys;
    }

    /// True while tiles are requested or waiting to be uploaded, including prefetching.
    #[cfg_attr(not(feature = "automation"), allow(dead_code))]
    pub fn has_pending_work(&self) -> bool {
        !self.last_wanted.is_empty() || !self.ready.is_empty()
    }

    /// True while there is work that needs more frames to show up.
    pub fn is_busy(&self) -> bool {
        !self.ready.is_empty()
    }

    pub fn clear(&mut self) {
        self.cache.clear();
        self.ready.clear();
        self.ready_keys.clear();
        self.failed.clear();
        self.last_wanted.clear();
    }

    #[cfg_attr(not(feature = "automation"), allow(dead_code))]
    pub fn stats(&self) -> &TileStats {
        &self.stats
    }

    pub fn status(&self) -> String {
        format!(
            "tiles    {} cached ({:.0} MB), {} waiting",
            self.cache.len(),
            self.cache.bytes() as f64 / (1024.0 * 1024.0),
            self.last_wanted.len() + self.ready.len()
        )
    }
}

/// Scale for a page's preview: [`PREVIEW_PX_PER_PT`], or less so the whole page fits in
/// a single tile.
pub fn preview_px_per_pt(width_pt: f32, height_pt: f32) -> f32 {
    let largest = width_pt.max(height_pt).max(1.0);
    PREVIEW_PX_PER_PT.min((TILE_SIZE - 1) as f32 / largest)
}

/// Wraps the tile's RGBA bytes as an egui image without copying them when possible.
fn color_image(tile: Tile) -> ColorImage {
    let size = [tile.width as usize, tile.height as usize];
    match bytemuck::allocation::try_cast_vec::<u8, Color32>(tile.rgba) {
        // Tiles are opaque, so straight and premultiplied alpha are the same.
        Ok(pixels) => ColorImage::new(size, pixels),
        Err((_, rgba)) => ColorImage::from_rgba_premultiplied(size, &rgba),
    }
}
