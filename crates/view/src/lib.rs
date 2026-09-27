//! Viewer logic that does not depend on the UI toolkit, so it can be unit tested.

pub mod camera;
pub mod layout;
pub mod scroll;
pub mod stats;
pub mod tiles;

pub use camera::{Camera, ZoomSettle};
pub use layout::{DocLayout, PageSlot};
pub use scroll::{AutoScroll, SmoothScroll};
pub use stats::{FrameSample, FrameStats, FrameSummary};
pub use tiles::{TileCache, visible_tiles};

/// Screen points (egui's logical pixels) per PDF point at 100% zoom: a 96 DPI screen shows
/// 96 pixels per inch and a PDF has 72 points per inch.
pub const SCREEN_PER_PT_AT_100: f32 = 96.0 / 72.0;
