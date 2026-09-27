//! The main window: toolbar, page canvas with scrollbar, and the frame timing HUD.

use std::path::PathBuf;
use std::sync::Arc;
use std::thread;

use crossbeam_channel::{Receiver, TryRecvError, bounded};
use eframe::egui::{
    self, Align, Align2, Color32, Event, FontId, Key, Layout, Modifiers, MouseWheelUnit, Rect,
    RichText, Sense, pos2, vec2,
};
use pdf_engine::geometry::{page_px_size, tile_rect};
use pdf_engine::{DocId, DocInfo, Engine, EngineConfig, PageSize, Scale, TILE_SIZE, TileKey};
use pdf_view::{AutoScroll, DocLayout, SCREEN_PER_PT_AT_100, SmoothScroll, visible_tiles};

use crate::hud::{Hud, HudAction};
use crate::tiles::{TileManager, preview_px_per_pt};

/// Screen points scrolled per mouse-wheel notch.
const WHEEL_STEP: f32 = 100.0;
/// Screen points scrolled per arrow key press.
const ARROW_STEP: f32 = 60.0;
const SCROLL_TEST_SECONDS: f32 = 8.0;
/// Screen points per second during the scroll test: fast, but a speed people really scroll at.
const SCROLL_TEST_SPEED: f32 = 2400.0;
const SCROLLBAR_WIDTH: f32 = 12.0;
const CANVAS_COLOR: Color32 = Color32::from_rgb(82, 86, 89);

struct Document {
    name: String,
    /// `None` for the built-in demo layout.
    id: Option<DocId>,
    layout: DocLayout,
}

type OpenResult = Result<(PathBuf, DocInfo), String>;

pub struct ViewerApp {
    engine: Result<Arc<Engine>, String>,
    document: Document,
    scroll: SmoothScroll,
    /// Percent. Fixed at 100 until zooming is added.
    zoom: f32,
    current_page: usize,
    hud: Hud,
    tiles: TileManager,
    auto_scroll: Option<AutoScroll>,
    opening: Option<Receiver<OpenResult>>,
    message: Option<String>,
    /// Set by `KRAKEN_SMOKE_TEST_FRAMES`: scroll for this many frames, then close. CI uses
    /// it to check that the window starts and draws on a real Windows machine.
    smoke_frames_left: Option<u32>,
}

impl ViewerApp {
    pub fn new(cc: &eframe::CreationContext, path: Option<PathBuf>) -> Self {
        let ctx = cc.egui_ctx.clone();
        let config = EngineConfig {
            // Enough parsed pages for everything on screen plus the pages prefetched around it.
            page_cache: 16,
            ..EngineConfig::default()
        };
        // Every finished tile wakes the UI, so it appears without waiting for input.
        let engine = Engine::start_with_waker(config, move || ctx.request_repaint())
            .map(Arc::new)
            .map_err(|e| e.to_string());
        let mut app = ViewerApp {
            message: engine.as_ref().err().cloned(),
            engine,
            document: demo_document(),
            scroll: SmoothScroll::default(),
            zoom: 100.0,
            current_page: 0,
            hud: Hud::new(),
            tiles: TileManager::new(),
            auto_scroll: None,
            opening: None,
            smoke_frames_left: std::env::var("KRAKEN_SMOKE_TEST_FRAMES")
                .ok()
                .and_then(|v| v.parse().ok()),
        };
        if app.smoke_frames_left.is_some() {
            app.auto_scroll = Some(AutoScroll::new(f32::MAX, SCROLL_TEST_SPEED));
        }
        if let Some(path) = path {
            app.open(path, &cc.egui_ctx);
        }
        app
    }

    fn screen_per_pt(&self) -> f32 {
        SCREEN_PER_PT_AT_100 * self.zoom / 100.0
    }

    /// Opens a PDF on a background thread so a large file cannot freeze the window.
    fn open(&mut self, path: PathBuf, ctx: &egui::Context) {
        let engine = match &self.engine {
            Ok(engine) => Arc::clone(engine),
            Err(e) => {
                self.message = Some(e.clone());
                return;
            }
        };
        let (tx, rx) = bounded(1);
        let ctx = ctx.clone();
        thread::spawn(move || {
            match engine.open(&path, None) {
                Ok(info) => {
                    let id = info.id;
                    // Nobody is waiting any more (another file was opened meanwhile).
                    if tx.send(Ok((path, info))).is_err() {
                        engine.close(id);
                    }
                }
                Err(e) => {
                    let _ = tx.send(Err(format!("{}: {e}", path.display())));
                }
            }
            ctx.request_repaint();
        });
        self.opening = Some(rx);
        self.message = Some("Opening...".into());
    }

    fn receive_opened(&mut self, ctx: &egui::Context) {
        let Some(rx) = &self.opening else { return };
        let result = match rx.try_recv() {
            Ok(result) => result,
            Err(TryRecvError::Empty) => return,
            Err(TryRecvError::Disconnected) => Err("opening the file failed".into()),
        };
        self.opening = None;
        match result {
            Ok((path, info)) => {
                if let (Ok(engine), Some(old)) = (&self.engine, self.document.id) {
                    engine.close(old);
                }
                let name = path.file_name().map_or_else(
                    || path.display().to_string(),
                    |n| n.to_string_lossy().into_owned(),
                );
                ctx.send_viewport_cmd(egui::ViewportCommand::Title(format!("{name} - Kraken PDF")));
                self.tiles.clear();
                self.document = Document {
                    name,
                    id: Some(info.id),
                    layout: DocLayout::new(&info.page_sizes),
                };
                self.scroll.jump_to(0.0);
                self.message = None;
            }
            Err(e) => self.message = Some(e),
        }
    }

    /// Returns `true` when the Open button was clicked.
    fn toolbar(&mut self, ui: &mut egui::Ui) -> bool {
        let mut open_clicked = false;
        ui.horizontal(|ui| {
            open_clicked = ui.button("Open...").on_hover_text("Ctrl+O").clicked();
            ui.separator();
            ui.label(RichText::new(&self.document.name).strong());
            let pages = self.document.layout.slots().len();
            if pages > 0 {
                ui.label(format!("Page {} / {pages}", self.current_page + 1));
            }
            if let Some(message) = &self.message {
                ui.separator();
                ui.label(RichText::new(message).color(Color32::from_rgb(255, 170, 120)));
            } else if self.document.id.is_none() {
                ui.separator();
                ui.label("Press Ctrl+O or drop a PDF on the window");
            }
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                ui.toggle_value(&mut self.hud.visible, "HUD (F3)");
            });
        });
        open_clicked
    }

    /// Draws the pages and scrollbar and handles scrolling. Returns `true` while moving.
    fn canvas(&mut self, ui: &mut egui::Ui) -> bool {
        let rect = ui.max_rect();
        let response = ui.allocate_rect(rect, Sense::hover());
        let s = self.screen_per_pt();
        let ppp = ui.ctx().pixels_per_point();
        let layout = &self.document.layout;
        let view_h = rect.height() / s;
        let max_scroll = (layout.height() - view_h).max(0.0);
        let dt = ui.input(|i| i.stable_dt).min(0.05);

        if response.hovered() {
            let wheel: Vec<_> = ui.input(|i| {
                i.events
                    .iter()
                    .filter_map(|e| match e {
                        // Ctrl+wheel is reserved for zooming.
                        Event::MouseWheel {
                            unit,
                            delta,
                            modifiers,
                            ..
                        } if !modifiers.command => Some((*unit, delta.y)),
                        _ => None,
                    })
                    .collect()
            });
            for (unit, dy) in wheel {
                // A positive delta moves the content down, i.e. scrolls toward the top.
                match unit {
                    // Touchpads send many small, already smooth steps.
                    MouseWheelUnit::Point => self.scroll.jump_by(-dy / s),
                    MouseWheelUnit::Line => self.scroll.scroll_by(-dy * WHEEL_STEP / s),
                    MouseWheelUnit::Page => self.scroll.scroll_by(-dy * view_h * 0.9),
                }
            }
        }

        let nothing_focused = ui.ctx().memory(|m| m.focused().is_none());
        if nothing_focused {
            let page_step = view_h * 0.9;
            ui.input(|i| {
                let mut delta = 0.0;
                if i.key_pressed(Key::ArrowDown) {
                    delta += ARROW_STEP / s;
                }
                if i.key_pressed(Key::ArrowUp) {
                    delta -= ARROW_STEP / s;
                }
                if i.key_pressed(Key::PageDown) || (i.key_pressed(Key::Space) && !i.modifiers.shift)
                {
                    delta += page_step;
                }
                if i.key_pressed(Key::PageUp) || (i.key_pressed(Key::Space) && i.modifiers.shift) {
                    delta -= page_step;
                }
                if delta != 0.0 {
                    self.scroll.scroll_by(delta);
                }
                if i.key_pressed(Key::Home) {
                    self.scroll.scroll_to(0.0);
                }
                if i.key_pressed(Key::End) {
                    self.scroll.scroll_to(max_scroll);
                }
            });
        }

        // Scrollbar: drag the thumb, or click the track to jump there.
        let bar = Rect::from_min_max(
            pos2(rect.right() - SCROLLBAR_WIDTH, rect.top()),
            rect.right_bottom(),
        );
        let bar_response = ui.interact(bar, ui.id().with("scrollbar"), Sense::click_and_drag());
        let total = layout.height().max(view_h);
        let thumb_h = (rect.height() * view_h / total)
            .max(32.0)
            .min(rect.height());
        let track = rect.height() - thumb_h;
        if track > 0.0 {
            if bar_response.dragged() {
                self.scroll
                    .jump_by(bar_response.drag_delta().y * max_scroll / track);
            } else if bar_response.clicked()
                && let Some(pointer) = bar_response.interact_pointer_pos()
            {
                let fraction = ((pointer.y - rect.top() - thumb_h / 2.0) / track).clamp(0.0, 1.0);
                self.scroll.scroll_to(fraction * max_scroll);
            }
        }

        if let Some(test) = &mut self.auto_scroll
            && !test.step(dt, &mut self.scroll, max_scroll)
        {
            self.auto_scroll = None;
            self.hud.finish_test();
        }
        self.scroll.clamp(max_scroll);
        let moving = self.scroll.update(dt);

        let painter = ui.painter_at(rect);
        painter.rect_filled(rect, 0.0, CANVAS_COLOR);
        let top = self.scroll.position();
        let doc_left =
            (rect.width() - SCROLLBAR_WIDTH) / 2.0 + rect.left() - layout.width() * s / 2.0;
        // Tiles are rendered at the screen's real pixel density, so they are drawn 1:1.
        let scale = Scale::from_zoom(self.zoom, ppp);
        let view_px = (rect.width() * ppp, rect.height() * ppp);
        let full_uv = Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0));

        // Pages one screen above and below are prepared too, so scrolling finds them ready.
        for index in layout.visible(top - view_h, top + 2.0 * view_h) {
            let slot = layout.slots()[index];
            let size = PageSize {
                width_pt: slot.width,
                height_pt: slot.height,
            };
            let page_px = page_px_size(size, scale);
            let min = pos2(doc_left + slot.x * s, rect.top() + (slot.y - top) * s);
            let page = snap_to_pixels(
                Rect::from_min_size(min, vec2(page_px.0 as f32, page_px.1 as f32) / ppp),
                ppp,
            );
            let on_screen = page.intersects(rect);
            // Lower numbers render first: previews of visible pages, then their sharp tiles,
            // then the same for the pages around them.
            let (preview_priority, tile_priority) = if on_screen {
                (0, 1_000)
            } else {
                (10_000, 20_000)
            };
            if on_screen {
                painter.rect_filled(
                    page.translate(vec2(0.0, 1.0)).expand(1.0),
                    1.0,
                    Color32::from_black_alpha(70),
                );
                painter.rect_filled(page, 0.0, Color32::WHITE);
            }

            let Some(doc) = self.document.id else {
                if on_screen {
                    painter.text(
                        page.center(),
                        Align2::CENTER_CENTER,
                        (index + 1).to_string(),
                        FontId::proportional(48.0),
                        Color32::from_gray(210),
                    );
                }
                continue;
            };
            let page_index = index as u32;
            let distance = (index as i64 - self.current_page as i64).unsigned_abs() as u32;

            let preview = TileKey {
                doc,
                page: page_index,
                scale: Scale::from_px_per_pt(preview_px_per_pt(slot.width, slot.height)),
                tx: 0,
                ty: 0,
            };
            if let Some(texture) = self.tiles.get(preview, preview_priority + distance)
                && on_screen
            {
                painter.image(texture.id(), page, full_uv, Color32::WHITE);
            }

            // Sharp tiles for the part of the page inside the extended view.
            let origin = (
                (page.min.x - rect.left()) * ppp,
                (page.min.y - rect.top()) * ppp,
            );
            let extended = (origin.0, origin.1 + view_px.1);
            let (cols, rows) = visible_tiles(page_px, extended, (view_px.0, view_px.1 * 3.0));
            let center = (view_px.0 / 2.0, view_px.1 / 2.0);
            for ty in rows {
                for tx in cols.clone() {
                    let Some(r) = tile_rect(page_px, tx, ty) else {
                        continue;
                    };
                    let tile_min = page.min + vec2(r.x as f32, r.y as f32) / ppp;
                    let tile_screen =
                        Rect::from_min_size(tile_min, vec2(r.width as f32, r.height as f32) / ppp);
                    let dx = origin.0 + (r.x + r.width / 2) as f32 - center.0;
                    let dy = origin.1 + (r.y + r.height / 2) as f32 - center.1;
                    let tiles_away = (dx.abs() + dy.abs()) / TILE_SIZE as f32;
                    let key = TileKey {
                        doc,
                        page: page_index,
                        scale,
                        tx,
                        ty,
                    };
                    if let Some(texture) = self.tiles.get(key, tile_priority + tiles_away as u32)
                        && tile_screen.intersects(rect)
                    {
                        painter.image(texture.id(), tile_screen, full_uv, Color32::WHITE);
                    }
                }
            }
        }
        self.current_page = layout.page_at(top + view_h / 2.0);

        let thumb_top = if max_scroll > 0.0 {
            rect.top() + track * top / max_scroll
        } else {
            rect.top()
        };
        painter.rect_filled(bar, 0.0, Color32::from_black_alpha(40));
        let thumb = Rect::from_min_size(
            pos2(bar.left() + 2.0, thumb_top + 2.0),
            vec2(SCROLLBAR_WIDTH - 4.0, thumb_h - 4.0),
        );
        let thumb_color = if bar_response.hovered() || bar_response.dragged() {
            Color32::from_gray(215)
        } else {
            Color32::from_gray(160)
        };
        painter.rect_filled(thumb, 4.0, thumb_color);

        moving
    }
}

impl eframe::App for ViewerApp {
    fn ui(&mut self, ui: &mut egui::Ui, frame: &mut eframe::Frame) {
        self.hud.begin_frame(frame.info().cpu_usage);
        let ctx = ui.ctx().clone();
        self.receive_opened(&ctx);
        if let Ok(engine) = &self.engine {
            self.tiles.begin_frame(engine, &ctx);
        }

        let (mut open_dialog, toggle_hud) = ctx.input_mut(|i| {
            (
                i.consume_key(Modifiers::COMMAND, Key::O),
                i.consume_key(Modifiers::NONE, Key::F3),
            )
        });
        if toggle_hud {
            self.hud.visible = !self.hud.visible;
        }
        let dropped = ctx.input(|i| i.raw.dropped_files.first().map(|f| f.path().to_path_buf()));
        if let Some(path) = dropped {
            self.open(path, &ctx);
        }

        egui::Panel::top("toolbar").show(ui, |ui| {
            open_dialog |= self.toolbar(ui);
        });
        let moving = egui::CentralPanel::default()
            .frame(egui::Frame::NONE)
            .show(ui, |ui| self.canvas(ui))
            .inner;

        if let Ok(engine) = &self.engine {
            self.tiles.end_frame(engine);
        }

        let tile_status = self.tiles.status();
        if let HudAction::RunScrollTest =
            self.hud
                .show(&ctx, self.auto_scroll.is_some(), &tile_status)
        {
            let speed = SCROLL_TEST_SPEED / self.screen_per_pt();
            self.auto_scroll = Some(AutoScroll::new(SCROLL_TEST_SECONDS, speed));
            self.hud.start_test();
        }

        if open_dialog
            && let Some(path) = rfd::FileDialog::new()
                .add_filter("PDF", &["pdf"])
                .pick_file()
        {
            self.open(path, &ctx);
        }

        if let Some(left) = &mut self.smoke_frames_left {
            if *left == 0 {
                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            }
            *left = left.saturating_sub(1);
        }

        let animating = moving || self.auto_scroll.is_some();
        self.hud.end_frame(animating);
        if animating || self.tiles.is_busy() {
            ctx.request_repaint();
        }
    }
}

/// Rounds a rectangle's corners to whole physical pixels so page edges stay sharp.
fn snap_to_pixels(rect: Rect, pixels_per_point: f32) -> Rect {
    let snap = |v: f32| (v * pixels_per_point).round() / pixels_per_point;
    Rect::from_min_max(
        pos2(snap(rect.min.x), snap(rect.min.y)),
        pos2(snap(rect.max.x), snap(rect.max.y)),
    )
}

/// 200 blank pages (every tenth one landscape) to try scrolling without opening a file.
fn demo_document() -> Document {
    let pages: Vec<PageSize> = (0..200)
        .map(|i| {
            let (width_pt, height_pt) = if i % 10 == 9 {
                (842.0, 595.0)
            } else {
                (595.0, 842.0)
            };
            PageSize {
                width_pt,
                height_pt,
            }
        })
        .collect();
    Document {
        name: "Demo: 200 blank pages".into(),
        id: None,
        layout: DocLayout::new(&pages),
    }
}
