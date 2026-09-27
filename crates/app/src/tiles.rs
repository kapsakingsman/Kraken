//! Keeps the page tiles on screen up to date: asks the engine for missing tiles, uploads
//! finished ones to the GPU (a few per frame), and keeps them in a memory-bounded cache.

use std::collections::{HashSet, VecDeque};

use eframe::egui::{self, Color32, ColorImage, TextureHandle, TextureOptions};
use pdf_engine::{Engine, TILE_SIZE, Tile, TileKey, TileRequest, TileResult};
use pdf_view::TileCache;

/// GPU memory for page tiles. A 512×512 tile takes 1 MB.
const MEMORY_BUDGET: usize = 300 * 1024 * 1024;

/// Uploading a tile costs about a millisecond of UI time, so spread them over frames to
/// stay inside the 144 Hz frame budget.
const UPLOADS_PER_FRAME: usize = 4;

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
}

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
        }
    }

    /// Collects finished tiles and uploads up to [`UPLOADS_PER_FRAME`] of them.
    pub fn begin_frame(&mut self, engine: &Engine, ctx: &egui::Context) {
        self.cache.begin_frame();
        for result in engine.results().try_iter() {
            self.ready_keys.insert(result.key);
            self.ready.push_back(result);
        }
        let mut uploaded = 0;
        while uploaded < UPLOADS_PER_FRAME {
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
                    uploaded += 1;
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
