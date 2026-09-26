//! PDF rendering engine.
//!
//! PDFium runs on one dedicated thread ([`Engine`]). Pages are rendered as square tiles of
//! [`TILE_SIZE`] pixels so that any zoom level costs the same memory per visible area, and
//! finished tiles are delivered as RGBA pixels ready to upload to the GPU.

mod actor;
mod error;
pub mod geometry;
mod library;
mod queue;

use std::time::Duration;

pub use actor::{Engine, EngineConfig};
pub use error::EngineError;
pub use geometry::{PageSize, Scale, TILE_SIZE, TileRect};
pub use library::locate_pdfium;

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
}

pub struct Tile {
    pub width: u32,
    pub height: u32,
    /// Tightly packed RGBA rows, `width * height * 4` bytes, opaque.
    pub rgba: Vec<u8>,
    pub render_time: Duration,
}

pub struct TileResult {
    pub key: TileKey,
    pub generation: u64,
    pub tile: Result<Tile, EngineError>,
}
