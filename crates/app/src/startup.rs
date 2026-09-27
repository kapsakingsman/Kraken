//! Getting the first page on screen quickly: PDFium starts, opens the file and renders the
//! first page while eframe is still creating the window and the GPU device (about a quarter
//! of a second), instead of after it.

use std::path::PathBuf;
use std::sync::{Arc, OnceLock};
use std::thread::{self, JoinHandle};

use crossbeam_channel::{Receiver, bounded};
use eframe::egui;
use pdf_engine::geometry::{page_px_size, tile_grid};
use pdf_engine::{
    DocInfo, EngineConfig, PoolConfig, Quality, RenderPool, Scale, TileKey, TileRequest,
    WorkerCommand,
};

use crate::tiles::preview_px_per_pt;

/// Zoom every document opens at.
pub const START_ZOOM: f32 = 100.0;

/// At most this many first-page tiles are rendered ahead (a large page at a high display
/// scale); the rest wait until the window knows what is visible.
const MAX_AHEAD_TILES: usize = 24;

pub struct Opened {
    pub path: PathBuf,
    pub info: DocInfo,
    /// Tiles already requested for the first page; their results must not be thrown away
    /// as unwanted before the view has asked for them.
    pub rendered_ahead: Vec<TileKey>,
}

pub type OpenResult = Result<Opened, String>;

/// What the startup thread prepared for the window.
pub struct Boot {
    pub engine: Result<Arc<RenderPool>, String>,
    pub opening: Option<Receiver<OpenResult>>,
}

/// Starts PDFium and, if a file was given, opens it and renders its first page, all on
/// background threads. `repaint` is filled in once the window exists; until then there is
/// nothing to wake.
pub fn begin(
    path: Option<PathBuf>,
    display_scale: Option<f32>,
    repaint: Arc<OnceLock<egui::Context>>,
) -> JoinHandle<Boot> {
    thread::spawn(move || {
        let config = PoolConfig {
            engine: EngineConfig {
                // Enough parsed pages for everything on screen plus the pages prefetched
                // around it.
                page_cache: 16,
                ..EngineConfig::default()
            },
            worker: worker_command(),
            ..PoolConfig::default()
        };
        let wake = Arc::clone(&repaint);
        // Every finished tile wakes the UI, so it appears without waiting for input.
        let engine = RenderPool::start(config, move || {
            if let Some(ctx) = wake.get() {
                ctx.request_repaint();
            }
        })
        .map(Arc::new)
        .map_err(|e| e.to_string());
        #[cfg(feature = "automation")]
        crate::automation::mark("pdfium_engine");
        let opening = match (&engine, path) {
            (Ok(engine), Some(path)) => Some(open_in_background(
                Arc::clone(engine),
                path,
                display_scale,
                move || {
                    if let Some(ctx) = repaint.get() {
                        ctx.request_repaint();
                    }
                },
            )),
            _ => None,
        };
        Boot { engine, opening }
    })
}

/// Opens a PDF on a background thread so a large file cannot freeze the window. With a
/// `display_scale`, the first page is also requested at the start zoom right away.
pub fn open_in_background(
    engine: Arc<RenderPool>,
    path: PathBuf,
    display_scale: Option<f32>,
    notify: impl Fn() + Send + 'static,
) -> Receiver<OpenResult> {
    let (tx, rx) = bounded(1);
    thread::spawn(move || {
        let result = match engine.open(&path, None) {
            Ok(info) => {
                let ahead = first_page_requests(&info, display_scale);
                let rendered_ahead = ahead.iter().map(|r| r.key).collect();
                if !ahead.is_empty() {
                    engine.set_wanted(0, ahead, None);
                }
                Ok(Opened {
                    path,
                    info,
                    rendered_ahead,
                })
            }
            Err(e) => Err(format!("{}: {e}", path.display())),
        };
        let id = result.as_ref().ok().map(|opened| opened.info.id);
        // Nobody is waiting any more (another file was opened meanwhile).
        if tx.send(result).is_err()
            && let Some(id) = id
        {
            engine.close(id);
        }
        notify();
    });
    rx
}

/// The app runs itself as its render workers, so there is one executable to ship.
fn worker_command() -> Option<WorkerCommand> {
    Some(WorkerCommand {
        program: std::env::current_exe().ok()?,
        args: vec![WORKER_FLAG.into()],
    })
}

/// Command-line flag that starts the app as a render worker instead of a window.
pub const WORKER_FLAG: &str = "--render-worker";

/// The first page's preview and, if the display scale is known, its sharp tiles at the
/// start zoom, with the same keys and priorities the view will ask for.
fn first_page_requests(info: &DocInfo, display_scale: Option<f32>) -> Vec<TileRequest> {
    let Some(&size) = info.page_sizes.first() else {
        return Vec::new();
    };
    let key = |scale, tx, ty| TileKey {
        doc: info.id,
        page: 0,
        scale,
        tx,
        ty,
        size: pdf_engine::TILE_SIZE,
    };
    let mut requests = vec![TileRequest {
        key: key(
            Scale::from_px_per_pt(preview_px_per_pt(size.width_pt, size.height_pt)),
            0,
            0,
        ),
        generation: 0,
        priority: 0,
        quality: Quality::Preview,
    }];
    if let Some(display_scale) = display_scale {
        let scale = Scale::from_zoom(START_ZOOM, display_scale);
        let (cols, rows) = tile_grid(page_px_size(size, scale), pdf_engine::TILE_SIZE);
        requests.extend(
            (0..rows)
                .flat_map(|ty| (0..cols).map(move |tx| (tx, ty)))
                .take(MAX_AHEAD_TILES)
                .map(|(tx, ty)| TileRequest {
                    key: key(scale, tx, ty),
                    generation: 0,
                    priority: 1_000 + tx + ty,
                    quality: Quality::Sharp,
                }),
        );
    }
    requests
}
