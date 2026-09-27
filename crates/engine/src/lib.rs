//! PDF rendering engine.
//!
//! PDFium runs on one dedicated thread ([`Engine`]). Pages are rendered as square tiles of
//! [`TILE_SIZE`] pixels so that any zoom level costs the same memory per visible area, and
//! finished tiles are delivered as RGBA pixels ready to upload to the GPU.

mod actor;
mod delivered;
mod error;
pub mod geometry;
mod library;
mod pool;
pub mod protocol;
mod queue;
mod system;
mod worker;

use std::time::Duration;

pub use actor::{Engine, EngineConfig};
pub use error::EngineError;
pub use geometry::{PageSize, Scale, TILE_SIZE, TileRect};
pub use library::locate_pdfium;
pub use pool::{PoolConfig, PoolStatus, RenderPool, WorkerCommand};
pub use worker::run_worker;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct DocId(pub u64);

#[derive(Clone, Debug)]
pub struct DocInfo {
    pub id: DocId,
    /// One entry per page, in points, with each page's rotation already applied.
    pub page_sizes: Vec<PageSize>,
}

/// Identifies one tile: which page, at which scale, and which column/row of the tile grid.
/// Page indices start at 0.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct TileKey {
    pub doc: DocId,
    pub page: u32,
    pub scale: Scale,
    pub tx: u32,
    pub ty: u32,
}

#[derive(Clone, Copy, Debug)]
pub struct TileRequest {
    pub key: TileKey,
    /// Requests older than the engine's current generation are skipped.
    pub generation: u64,
    /// Lower values render first, e.g. distance from the center of the screen.
    pub priority: u32,
    pub quality: Quality,
}

/// How carefully to render a tile.
///
/// PDFium's high-quality image downscaling ("image smoothing") dominates render time on
/// pages with very large images: one page with a 151-megapixel image mask took 4.9 s with it
/// and 0.24 s without. It only changes how images look, so text and vector pages are the
/// same either way.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Quality {
    /// Low-resolution previews: image smoothing off.
    Preview,
    /// On pages with images, a quick draft with image smoothing off
    /// ([`Tile::draft`] is set); ask for [`Quality::Final`] afterwards. Other pages are
    /// rendered final right away.
    Sharp,
    /// Full quality.
    Final,
}

pub struct Tile {
    pub width: u32,
    pub height: u32,
    /// Tightly packed RGBA rows, `width * height * 4` bytes, opaque.
    pub rgba: Vec<u8>,
    pub render_time: Duration,
    /// Rendered without image smoothing; a [`Quality::Final`] render would look better.
    pub draft: bool,
}

pub struct TileResult {
    pub key: TileKey,
    pub generation: u64,
    pub tile: Result<Tile, EngineError>,
}
