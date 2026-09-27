//! The engine thread: the only thread that ever calls into PDFium.

use std::collections::HashMap;
use std::path::PathBuf;
use std::thread::{self, JoinHandle};
use std::time::Instant;

use crossbeam_channel::{Receiver, Sender, bounded, unbounded};
use pdfium_render::prelude::*;

use crate::geometry::{self, PageSize, TILE_SIZE, TileRect};
use crate::queue::TileQueue;
use crate::{DocId, DocInfo, EngineError, Quality, Tile, TileKey, TileRequest, TileResult};

pub struct EngineConfig {
    /// Folder containing the PDFium library. `None` uses [`crate::locate_pdfium`].
    pub pdfium_dir: Option<PathBuf>,
    /// How many parsed pages to keep per document. Parsing is the slow part of rendering a
    /// page, so the pages currently on screen should all fit.
    pub page_cache: usize,
}

impl Default for EngineConfig {
    fn default() -> Self {
        EngineConfig {
            pdfium_dir: None,
            page_cache: 8,
        }
    }
}

enum Command {
    Open {
        path: PathBuf,
        password: Option<String>,
        reply: Sender<Result<DocInfo, EngineError>>,
    },
    Close(DocId),
    Tile(TileRequest),
    SetGeneration(u64),
    Shutdown,
}

/// Handle to the engine thread. PDFium is not thread safe, so every PDFium call happens on
/// that one thread; callers only exchange messages with it and never block on rendering.
pub struct Engine {
    commands: Sender<Command>,
    results: Receiver<TileResult>,
    thread: Option<JoinHandle<()>>,
}

impl Engine {
    pub fn start(config: EngineConfig) -> Result<Self, EngineError> {
        Self::start_with_waker(config, || {})
    }

    /// `waker` is called on the engine thread after each finished tile. A UI passes its
    /// "request repaint" function here so new tiles show up without waiting for input.
    pub fn start_with_waker(
        config: EngineConfig,
        waker: impl Fn() + Send + 'static,
    ) -> Result<Self, EngineError> {
        let lib_dir = match config.pdfium_dir {
            Some(dir) => dir,
            None => crate::locate_pdfium()?,
        };
        let (command_tx, command_rx) = unbounded();
        let (result_tx, result_rx) = unbounded();
        let (ready_tx, ready_rx) = bounded(1);
        let page_cache = config.page_cache.max(1);

        let thread = thread::Builder::new()
            .name("pdfium".into())
            .spawn(move || {
                let lib_path = Pdfium::pdfium_platform_library_name_at_path(&lib_dir);
                let pdfium = match Pdfium::bind_to_library(&lib_path) {
                    Ok(bindings) => Pdfium::new(bindings),
                    // PDFium can be loaded only once per process; share the loaded copy.
                    Err(PdfiumError::PdfiumLibraryBindingsAlreadyInitialized) => Pdfium::default(),
                    Err(e) => {
                        let _ = ready_tx.send(Err(EngineError::LibraryLoad {
                            path: lib_path,
                            reason: format!("{e:?}"),
                        }));
                        return;
                    }
                };
                let _ = ready_tx.send(Ok(()));
                Worker {
                    pdfium: &pdfium,
                    commands: command_rx,
                    results: result_tx,
                    waker: Box::new(waker),
                    docs: HashMap::new(),
                    queue: TileQueue::default(),
                    generation: 0,
                    next_doc: 1,
                    page_cache,
                    bitmap: None,
                }
                .run();
            })?;

        ready_rx.recv().map_err(|_| EngineError::Stopped)??;
        Ok(Engine {
            commands: command_tx,
            results: result_rx,
            thread: Some(thread),
        })
    }

    /// Opens a PDF and returns its page sizes. This waits for the engine thread, so a UI
    /// should call it from a background thread for large files.
    pub fn open(
        &self,
        path: impl Into<PathBuf>,
        password: Option<&str>,
    ) -> Result<DocInfo, EngineError> {
        let (reply, response) = bounded(1);
        self.send(Command::Open {
            path: path.into(),
            password: password.map(str::to_owned),
            reply,
        })?;
        response.recv().map_err(|_| EngineError::Stopped)?
    }

    pub fn close(&self, doc: DocId) {
        let _ = self.send(Command::Close(doc));
    }

    /// Queues a tile. The result arrives on [`Engine::results`].
    pub fn request_tile(&self, request: TileRequest) {
        let _ = self.send(Command::Tile(request));
    }

    /// Drops queued tiles from older generations. Bump the generation when the zoom level
    /// changes so the engine stops working on tiles nobody will see.
    pub fn set_generation(&self, generation: u64) {
        let _ = self.send(Command::SetGeneration(generation));
    }

    pub fn results(&self) -> &Receiver<TileResult> {
        &self.results
    }

    fn send(&self, command: Command) -> Result<(), EngineError> {
        self.commands
            .send(command)
            .map_err(|_| EngineError::Stopped)
    }
}

impl Drop for Engine {
    fn drop(&mut self) {
        let _ = self.commands.send(Command::Shutdown);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

struct Worker<'p> {
    pdfium: &'p Pdfium,
    commands: Receiver<Command>,
    results: Sender<TileResult>,
    waker: Box<dyn Fn() + Send>,
    docs: HashMap<DocId, OpenDoc<'p>>,
    queue: TileQueue,
    generation: u64,
    next_doc: u64,
    page_cache: usize,
    /// Reused for every tile so rendering does not allocate a new 1 MB buffer each time.
    bitmap: Option<PdfBitmap<'p>>,
}

struct OpenDoc<'p> {
    // Declared before `document` so cached pages are closed before their document.
    pages: Vec<(u32, PdfPage<'p>)>,
    document: PdfDocument<'p>,
    sizes: Vec<PageSize>,
    /// Whether each page draws images, found out the first time it is rendered.
    has_images: HashMap<u32, bool>,
}

impl<'p> Worker<'p> {
    fn run(mut self) {
        loop {
            if self.queue.is_empty() {
                match self.commands.recv() {
                    Ok(command) => {
                        if !self.handle(command) {
                            return;
                        }
                    }
                    Err(_) => return,
                }
            }
            // Take everything that arrived meanwhile, so the next tile is picked using the
            // latest priorities and generation.
            while let Ok(command) = self.commands.try_recv() {
                if !self.handle(command) {
                    return;
                }
            }
            if let Some(request) = self.queue.pop(self.generation) {
                let tile = self.render(&request.key, request.quality);
                let result = TileResult {
                    key: request.key,
                    generation: request.generation,
                    tile,
                };
                if self.results.send(result).is_err() {
                    return;
                }
                (self.waker)();
            }
        }
    }

    /// Returns `false` when the thread should stop.
    fn handle(&mut self, command: Command) -> bool {
        match command {
            Command::Open {
                path,
                password,
                reply,
            } => {
                let _ = reply.send(self.open(path, password.as_deref()));
            }
            Command::Close(doc) => {
                self.docs.remove(&doc);
                self.queue.remove_doc(doc);
            }
            Command::Tile(request) => {
                if request.generation >= self.generation {
                    self.queue.push(request);
                }
            }
            Command::SetGeneration(generation) => {
                self.generation = self.generation.max(generation);
                self.queue.drop_older_than(self.generation);
            }
            Command::Shutdown => return false,
        }
        true
    }

    fn open(&mut self, path: PathBuf, password: Option<&str>) -> Result<DocInfo, EngineError> {
        let document = self
            .pdfium
            .load_pdf_from_file(&path, password)
            .map_err(EngineError::from_open)?;
        let sizes: Vec<PageSize> = document
            .pages()
            .page_sizes()?
            .iter()
            .map(|rect| PageSize {
                width_pt: rect.width().value,
                height_pt: rect.height().value,
            })
            .collect();

        let id = DocId(self.next_doc);
        self.next_doc += 1;
        self.docs.insert(
            id,
            OpenDoc {
                pages: Vec::new(),
                document,
                sizes: sizes.clone(),
                has_images: HashMap::new(),
            },
        );
        Ok(DocInfo {
            id,
            page_sizes: sizes,
        })
    }

    fn render(&mut self, key: &TileKey, quality: Quality) -> Result<Tile, EngineError> {
        let started = Instant::now();
        let doc = self
            .docs
            .get_mut(&key.doc)
            .ok_or(EngineError::UnknownDocument)?;
        let size = *doc
            .sizes
            .get(key.page as usize)
            .ok_or(EngineError::PageOutOfRange(key.page))?;
        let page_px = geometry::page_px_size(size, key.scale);
        let rect =
            geometry::tile_rect(page_px, key.tx, key.ty).ok_or(EngineError::TileOutOfRange)?;
        let known_images = doc.has_images.get(&key.page).copied();
        let page = doc.page(key.page, self.page_cache)?;
        let has_images = known_images.unwrap_or_else(|| page_has_images(page));
        let draft = match quality {
            Quality::Preview => true,
            Quality::Final => false,
            Quality::Sharp => has_images,
        };

        let bitmap = match &mut self.bitmap {
            Some(bitmap) => bitmap,
            empty => empty.insert(PdfBitmap::empty(
                TILE_SIZE as Pixels,
                TILE_SIZE as Pixels,
                PdfBitmapFormat::BGRA,
            )?),
        };
        let rgba = render_tile(page, page_px, rect, bitmap, !draft)?;
        doc.has_images.insert(key.page, has_images);
        Ok(Tile {
            width: rect.width,
            height: rect.height,
            rgba,
            render_time: started.elapsed(),
            // A preview is not refined, so it is not reported as a draft.
            draft: draft && quality == Quality::Sharp,
        })
    }
}

impl<'p> OpenDoc<'p> {
    /// Returns a parsed page, loading it if needed and evicting the least recently used one.
    fn page(&mut self, index: u32, capacity: usize) -> Result<&PdfPage<'p>, EngineError> {
        if let Some(pos) = self.pages.iter().position(|(i, _)| *i == index) {
            let entry = self.pages.remove(pos);
            self.pages.push(entry);
        } else {
            let page = self.document.pages().get(index as PdfPageIndex)?;
            if self.pages.len() >= capacity {
                self.pages.remove(0);
            }
            self.pages.push((index, page));
        }
        Ok(&self.pages.last().expect("page was just inserted").1)
    }
}

/// Renders one tile. Instead of a transformation matrix, the page is drawn at its full pixel
/// size and shifted so the tile's corner lands at the bitmap origin; PDFium clips everything
/// outside the bitmap. Unlike the matrix path, this also draws form fields and supports
/// PDFium's progressive (pausable) rendering.
fn render_tile(
    page: &PdfPage,
    page_px: (u32, u32),
    rect: TileRect,
    bitmap: &mut PdfBitmap,
    smooth_images: bool,
) -> Result<Vec<u8>, EngineError> {
    let config = PdfRenderConfig::new()
        .set_image_smoothing(smooth_images)
        .set_fixed_size(page_px.0 as Pixels, page_px.1 as Pixels)
        .set_origin(-(rect.x as Pixels), -(rect.y as Pixels))
        .render_form_data(true)
        .render_annotations(true)
        .set_clear_color(PdfColor::WHITE)
        // PDFium writes BGRA by default; this makes it write RGBA so nobody has to swap bytes.
        .set_reverse_byte_order(true);
    page.render_into_bitmap_with_config(bitmap, &config)?;

    let raw = bitmap.as_raw_bytes();
    let stride = raw.len() / TILE_SIZE as usize;
    let row_bytes = rect.width as usize * 4;
    let mut rgba = Vec::with_capacity(row_bytes * rect.height as usize);
    for row in raw.chunks_exact(stride).take(rect.height as usize) {
        rgba.extend_from_slice(&row[..row_bytes]);
    }
    Ok(rgba)
}

/// Whether the page draws any image, including images inside form XObjects.
fn page_has_images(page: &PdfPage) -> bool {
    page.objects()
        .iter()
        .any(|object| object_has_images(&object, 0))
}

fn object_has_images(object: &PdfPageObject, depth: u32) -> bool {
    match object.object_type() {
        PdfPageObjectType::Image => true,
        // Forms can nest; the depth limit guards against malicious self-references.
        PdfPageObjectType::XObjectForm if depth < 16 => {
            object.as_x_object_form_object().is_some_and(|form| {
                (0..form.len())
                    .filter_map(|i| form.get(i).ok())
                    .any(|child| object_has_images(&child, depth + 1))
            })
        }
        _ => false,
    }
}
