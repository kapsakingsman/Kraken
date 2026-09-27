//! The main window: toolbar, page canvas with scrollbars, and the frame timing HUD.

use std::path::PathBuf;
use std::sync::Arc;

use crossbeam_channel::{Receiver, TryRecvError};
use eframe::egui::{
    self, Align, Align2, Color32, Event, FontId, Key, Layout, Modifiers, MouseWheelUnit, Painter,
    Rect, RichText, Sense, Vec2, pos2, vec2,
};
use pdf_engine::geometry::{page_px_size, tile_rect};
use pdf_engine::{DocId, PageSize, Quality, RenderPool, Scale, TILE_SIZE, TileKey};
use pdf_view::camera::{fit_page_zoom, fit_width_zoom, step_zoom, wheel_notches};
use pdf_view::{AutoScroll, Camera, DocLayout, SmoothScroll, ZoomSettle, visible_tiles};

use crate::hud::{Hud, HudAction};
use crate::settings;
use crate::startup::{Boot, OpenResult, Opened, START_ZOOM, open_in_background};
use crate::tiles::{TileManager, preview_px_per_pt};

/// Screen points scrolled per mouse-wheel notch.
const WHEEL_STEP: f32 = 100.0;
/// Screen points scrolled per arrow key press.
const ARROW_STEP: f32 = 60.0;
const SCROLL_TEST_SECONDS: f32 = 8.0;
/// Screen points per second during the scroll test: fast, but a speed people really scroll at.
const SCROLL_TEST_SPEED: f32 = 2400.0;
const SCROLLBAR_WIDTH: f32 = 12.0;
/// Space kept around the page by Fit Width and Fit Page, in screen points.
const FIT_MARGIN: f32 = 16.0;
const CANVAS_COLOR: Color32 = Color32::from_rgb(82, 86, 89);

struct Document {
    name: String,
    /// `None` for the built-in demo layout.
    id: Option<DocId>,
    layout: DocLayout,
}

/// Zoom modes that follow the window size until the user zooms by hand.
#[derive(Clone, Copy, PartialEq)]
enum Fit {
    Width,
    Page,
}

enum ZoomCommand {
    StepIn,
    StepOut,
    Set(f32),
    Fit(Fit),
}

pub struct ViewerApp {
    engine: Result<Arc<RenderPool>, String>,
    /// Render workers were started (once the first page was on screen).
    prewarmed: bool,
    document: Document,
    camera: Camera,
    settle: ZoomSettle,
    fit: Option<Fit>,
    zoom_command: Option<ZoomCommand>,
    /// Scale of the last tile set that was complete on screen. Its tiles are drawn,
    /// stretched, until the tiles for a new zoom arrive.
    fallback_scale: Option<Scale>,
    render_scale: Option<Scale>,
    render_complete: bool,
    current_page: usize,
    hud: Hud,
    tiles: TileManager,
    auto_scroll: Option<AutoScroll>,
    opening: Option<Receiver<OpenResult>>,
    message: Option<String>,
    /// The display scale remembered for the next start (see `settings`).
    saved_display_scale: Option<f32>,
    #[cfg(feature = "automation")]
    automation: Option<crate::automation::Automation>,
}

impl ViewerApp {
    pub fn new(
        cc: &eframe::CreationContext,
        boot: Boot,
        gpu: String,
        saved_display_scale: Option<f32>,
    ) -> Self {
        // Ctrl+plus/minus zoom the document, not egui's own UI scale.
        cc.egui_ctx.options_mut(|o| o.zoom_with_keyboard = false);
        let message = match (&boot.engine, &boot.opening) {
            (Err(e), _) => Some(e.clone()),
            (Ok(_), Some(_)) => Some("Opening...".into()),
            (Ok(_), None) => None,
        };
        ViewerApp {
            message,
            engine: boot.engine,
            prewarmed: false,
            document: demo_document(),
            camera: Camera::default(),
            settle: ZoomSettle::new(START_ZOOM),
            fit: None,
            zoom_command: None,
            fallback_scale: None,
            render_scale: None,
            render_complete: false,
            current_page: 0,
            hud: Hud::new(gpu.clone()),
            tiles: TileManager::new(),
            auto_scroll: None,
            opening: boot.opening,
            saved_display_scale,
            #[cfg(feature = "automation")]
            automation: crate::automation::Automation::from_env(gpu),
        }
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
        let ctx = ctx.clone();
        self.opening = Some(open_in_background(engine, path, None, move || {
            ctx.request_repaint();
        }));
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
            Ok(Opened {
                path,
                info,
                rendered_ahead,
            }) => {
                if let (Ok(engine), Some(old)) = (&self.engine, self.document.id) {
                    engine.close(old);
                }
                let name = path.file_name().map_or_else(
                    || path.display().to_string(),
                    |n| n.to_string_lossy().into_owned(),
                );
                ctx.send_viewport_cmd(egui::ViewportCommand::Title(format!("{name} - Kraken PDF")));
                self.tiles.clear();
                self.tiles.expect(rendered_ahead);
                self.fallback_scale = None;
                self.document = Document {
                    name,
                    id: Some(info.id),
                    layout: DocLayout::new(&info.page_sizes),
                };
                self.camera.x.jump_to(0.0);
                self.camera.y.jump_to(0.0);
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
            if ui
                .button("−")
                .on_hover_text("Zoom out (Ctrl+minus)")
                .clicked()
            {
                self.zoom_command = Some(ZoomCommand::StepOut);
            }
            ui.label(RichText::new(format!("{:.0}%", self.camera.zoom())).monospace());
            if ui
                .button("+")
                .on_hover_text("Zoom in (Ctrl+plus)")
                .clicked()
            {
                self.zoom_command = Some(ZoomCommand::StepIn);
            }
            if ui
                .selectable_label(self.fit == Some(Fit::Width), "Fit width")
                .on_hover_text("Ctrl+2")
                .clicked()
            {
                self.zoom_command = Some(ZoomCommand::Fit(Fit::Width));
            }
            if ui
                .selectable_label(self.fit == Some(Fit::Page), "Fit page")
                .on_hover_text("Ctrl+0")
                .clicked()
            {
                self.zoom_command = Some(ZoomCommand::Fit(Fit::Page));
            }
            if ui.button("100%").on_hover_text("Ctrl+1").clicked() {
                self.zoom_command = Some(ZoomCommand::Set(100.0));
            }
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

    fn fit_zoom(&self, fit: Fit, view: Vec2) -> f32 {
        let usable = vec2(
            view.x - SCROLLBAR_WIDTH - 2.0 * FIT_MARGIN,
            view.y - 2.0 * FIT_MARGIN,
        );
        let layout = &self.document.layout;
        match fit {
            Fit::Width => fit_width_zoom(layout.width(), usable.x),
            Fit::Page => match layout.slots().get(self.current_page) {
                Some(slot) => fit_page_zoom((slot.width, slot.height), (usable.x, usable.y)),
                None => 100.0,
            },
        }
    }

    /// Handles zoom and scroll input, then draws the pages and scrollbars.
    /// Returns `true` while something is moving.
    fn canvas(&mut self, ui: &mut egui::Ui) -> bool {
        let rect = ui.max_rect();
        let response = ui.allocate_rect(rect, Sense::hover());
        let ctx = ui.ctx().clone();
        let ppp = ctx.pixels_per_point();
        let view = (rect.width(), rect.height());
        let content = (self.document.layout.width(), self.document.layout.height());
        let center = (view.0 / 2.0, view.1 / 2.0);
        let dt = ui.input(|i| i.stable_dt).min(0.05);

        // --- Zoom ---------------------------------------------------------------------
        if let Some(command) = self.zoom_command.take() {
            let zoom = self.camera.zoom();
            let (target, fit) = match command {
                ZoomCommand::StepIn => (step_zoom(zoom, 1), None),
                ZoomCommand::StepOut => (step_zoom(zoom, -1), None),
                ZoomCommand::Set(z) => (z, None),
                ZoomCommand::Fit(fit) => (self.fit_zoom(fit, rect.size()), Some(fit)),
            };
            self.fit = fit;
            self.camera.zoom_around(target, center, content, view);
            if fit == Some(Fit::Page) {
                let top = self.document.layout.page_top(self.current_page);
                self.camera.y.jump_to(top);
            }
            // A deliberate jump to a new zoom: render it right away.
            self.settle.snap(self.camera.zoom());
        }
        if let Some(fit) = self.fit {
            // Keep following the window size.
            let target = self.fit_zoom(fit, rect.size());
            if (target - self.camera.zoom()).abs() > 0.05 {
                self.camera.zoom_around(target, center, content, view);
            }
        }
        // Ctrl+wheel and pinch. egui's `zoom_delta` smooths Ctrl+wheel over ~0.15 s, so the
        // zoom would keep changing after the last notch and only then render sharp. Like
        // Acrobat, a mouse-wheel notch instead jumps to the next zoom step and renders it
        // right away. Touchpad pinches and fractional wheel deltas zoom smoothly, stretching
        // the current tiles until the gesture settles.
        let mut zooming = false;
        if response.hovered() {
            let (notches, factor, hover) = ui.input(|i| {
                let mut notches = 0;
                let mut factor = 1.0;
                for event in &i.events {
                    match event {
                        Event::MouseWheel {
                            unit,
                            delta,
                            modifiers,
                            ..
                        } if modifiers.command => {
                            let d = delta.x + delta.y;
                            match (unit, wheel_notches(d)) {
                                (MouseWheelUnit::Line, Some(n)) => notches += n,
                                (MouseWheelUnit::Line, None) => factor *= (d * 0.2).exp(),
                                (MouseWheelUnit::Point, _) => factor *= (d / 200.0).exp(),
                                (MouseWheelUnit::Page, _) => notches += d.signum() as i32,
                            }
                        }
                        Event::Zoom(f) if i.multi_touch().is_none() => factor *= f,
                        _ => {}
                    }
                }
                if let Some(touch) = i.multi_touch() {
                    factor *= touch.zoom_delta;
                }
                (notches, factor, i.pointer.hover_pos())
            });
            let anchor = hover.map_or(center, |p| (p.x - rect.left(), p.y - rect.top()));
            if notches != 0 {
                let mut target = self.camera.zoom();
                for _ in 0..notches.unsigned_abs() {
                    target = step_zoom(target, notches.signum());
                }
                self.camera.zoom_around(target, anchor, content, view);
                self.settle.snap(self.camera.zoom());
                self.fit = None;
                zooming = true;
            }
            if factor != 1.0 {
                self.camera
                    .zoom_around(self.camera.zoom() * factor, anchor, content, view);
                self.fit = None;
                zooming = true;
            }
        }

        // --- Scroll -------------------------------------------------------------------
        let s = self.camera.screen_per_pt();
        let view_h_pt = view.1 / s;
        if response.hovered() {
            let wheel: Vec<_> = ui.input(|i| {
                i.events
                    .iter()
                    .filter_map(|e| match e {
                        // Ctrl+wheel is zoom, handled above.
                        Event::MouseWheel {
                            unit,
                            delta,
                            modifiers,
                            ..
                        } if !modifiers.command => Some((*unit, *delta, modifiers.shift)),
                        _ => None,
                    })
                    .collect()
            });
            for (unit, delta, shift) in wheel {
                // A positive delta moves the content down/right, i.e. scrolls up/left.
                let delta = if shift && delta.x == 0.0 {
                    vec2(delta.y, 0.0) // Shift+wheel scrolls sideways
                } else {
                    delta
                };
                match unit {
                    // Touchpads send many small, already smooth steps.
                    MouseWheelUnit::Point => {
                        self.camera.x.jump_by(-delta.x / s);
                        self.camera.y.jump_by(-delta.y / s);
                    }
                    MouseWheelUnit::Line => {
                        self.camera.x.scroll_by(-delta.x * WHEEL_STEP / s);
                        self.camera.y.scroll_by(-delta.y * WHEEL_STEP / s);
                    }
                    MouseWheelUnit::Page => {
                        self.camera.y.scroll_by(-delta.y * view_h_pt * 0.9);
                    }
                }
            }
        }

        let (max_x, max_y) = self.camera.max_scroll(content, view);
        if ctx.memory(|m| m.focused().is_none()) {
            let page_step = view_h_pt * 0.9;
            ui.input(|i| {
                let mut dy = 0.0;
                let mut dx = 0.0;
                if i.key_pressed(Key::ArrowDown) {
                    dy += ARROW_STEP / s;
                }
                if i.key_pressed(Key::ArrowUp) {
                    dy -= ARROW_STEP / s;
                }
                if i.key_pressed(Key::ArrowRight) {
                    dx += ARROW_STEP / s;
                }
                if i.key_pressed(Key::ArrowLeft) {
                    dx -= ARROW_STEP / s;
                }
                if i.key_pressed(Key::PageDown) || (i.key_pressed(Key::Space) && !i.modifiers.shift)
                {
                    dy += page_step;
                }
                if i.key_pressed(Key::PageUp) || (i.key_pressed(Key::Space) && i.modifiers.shift) {
                    dy -= page_step;
                }
                if dy != 0.0 {
                    self.camera.y.scroll_by(dy);
                }
                if dx != 0.0 {
                    self.camera.x.scroll_by(dx);
                }
                if i.key_pressed(Key::Home) {
                    self.camera.y.scroll_to(0.0);
                }
                if i.key_pressed(Key::End) {
                    self.camera.y.scroll_to(max_y);
                }
            });
        }

        let v_bar = Rect::from_min_max(
            pos2(rect.right() - SCROLLBAR_WIDTH, rect.top()),
            rect.right_bottom(),
        );
        let v_thumb = scrollbar(
            ui,
            v_bar,
            false,
            &mut self.camera.y,
            max_y,
            view_h_pt / content.1.max(view_h_pt),
        );
        let h_bar = Rect::from_min_max(
            pos2(rect.left(), rect.bottom() - SCROLLBAR_WIDTH),
            pos2(rect.right() - SCROLLBAR_WIDTH, rect.bottom()),
        );
        let h_thumb = (max_x > 0.0).then(|| {
            let view_w_pt = view.0 / s;
            scrollbar(
                ui,
                h_bar,
                true,
                &mut self.camera.x,
                max_x,
                view_w_pt / content.0.max(view_w_pt),
            )
        });

        #[cfg(feature = "automation")]
        let zoom_settled = self.settle.render_zoom() == self.camera.zoom();
        #[cfg(feature = "automation")]
        let automating = match &mut self.automation {
            Some(automation) => automation.drive(
                &mut self.camera,
                &crate::automation::FrameState {
                    document_open: self.document.id.is_some(),
                    render_complete: self.render_complete,
                    tiles_pending: self.tiles.has_pending_work(),
                    zoom_settled,
                    content,
                    view,
                    dt,
                },
            ),
            None => false,
        };
        #[cfg(not(feature = "automation"))]
        let automating = false;

        if let Some(test) = &mut self.auto_scroll
            && !test.step(dt, &mut self.camera.y, max_y)
        {
            self.auto_scroll = None;
            self.hud.finish_test();
        }
        self.camera.clamp(content, view);
        let moving = self.camera.update(dt);

        // --- Which scale do tiles render at? --------------------------------------------
        if let Some(wait) = self.settle.update(self.camera.zoom(), ui.input(|i| i.time)) {
            ctx.request_repaint_after_secs(wait as f32);
        }
        let render_scale = Scale::from_zoom(self.settle.render_zoom(), ppp);
        if self.render_scale != Some(render_scale) {
            // Keep the previous tiles as a stand-in, unless they never finished loading
            // (then the older, complete set is the better stand-in).
            if self.render_complete || self.fallback_scale.is_none() {
                self.fallback_scale = self.render_scale;
            }
            self.render_scale = Some(render_scale);
            self.render_complete = false;
        }
        let display_scale = Scale::from_zoom(self.camera.zoom(), ppp);

        // --- Paint ---------------------------------------------------------------------
        let painter = ui.painter_at(rect);
        painter.rect_filled(rect, 0.0, CANVAS_COLOR);
        let s = self.camera.screen_per_pt();
        let top = self.camera.y.position();
        let doc_left = rect.left() + self.camera.doc_left_pt(content.0, view.0) * s;
        let view_px = (view.0 * ppp, view.1 * ppp);
        let mut missing = 0;

        // Pages one screen above and below are prepared too, so scrolling finds them ready.
        let layout = &self.document.layout;
        for index in layout.visible(top - view_h_pt, top + 2.0 * view_h_pt) {
            let slot = layout.slots()[index];
            let size = PageSize {
                width_pt: slot.width,
                height_pt: slot.height,
            };
            let page_px = page_px_size(size, display_scale);
            let min = pos2(doc_left + slot.x * s, rect.top() + (slot.y - top) * s);
            let page = snap_to_pixels(
                Rect::from_min_size(min, vec2(page_px.0 as f32, page_px.1 as f32) / ppp),
                ppp,
            );
            let on_screen = page.intersects(rect);
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
                        FontId::proportional(48.0 * s / 1.333),
                        Color32::from_gray(210),
                    );
                }
                continue;
            };
            // Lower numbers render first: previews of visible pages, then their sharp tiles,
            // then the same for the pages around them.
            let (preview_priority, tile_priority) = if on_screen {
                (0, 1_000)
            } else {
                (10_000, 20_000)
            };
            let distance = index.abs_diff(self.current_page) as u32;
            let preview = TileKey {
                doc,
                page: index as u32,
                scale: Scale::from_px_per_pt(preview_px_per_pt(slot.width, slot.height)),
                tx: 0,
                ty: 0,
            };
            if let Some(texture) =
                self.tiles
                    .get(preview, preview_priority + distance, Quality::Preview)
                && on_screen
            {
                painter.image(texture.id(), page, FULL_UV, Color32::WHITE);
            }

            let mut draw = |scale: Scale, priority: Option<u32>| {
                draw_page_tiles(
                    &mut self.tiles,
                    &painter,
                    PageTiles {
                        doc,
                        page: index as u32,
                        size,
                        page_rect: page,
                        viewport: rect,
                        ppp,
                        view_px,
                        scale,
                        priority,
                    },
                )
            };
            if let Some(fallback) = self.fallback_scale
                && fallback != render_scale
            {
                draw(fallback, None);
            }
            // While a zoom gesture is in progress the existing tiles are stretched; asking
            // for more tiles at a scale about to be replaced would only waste rendering.
            let settled = render_scale == display_scale;
            missing += draw(render_scale, settled.then_some(tile_priority));
        }
        self.current_page = layout.page_at(top + view_h_pt / 2.0);
        if missing == 0 && self.document.id.is_some() {
            self.render_complete = true;
            self.fallback_scale = None;
            // The first page is on screen: now the render workers can start without
            // slowing down startup.
            if !self.prewarmed
                && let Ok(engine) = &self.engine
            {
                engine.prewarm();
                self.prewarmed = true;
            }
        }

        paint_scrollbar(&painter, v_bar, v_thumb);
        if let Some(thumb) = h_thumb {
            paint_scrollbar(&painter, h_bar, thumb);
        }

        moving || zooming || automating
    }
}

impl eframe::App for ViewerApp {
    #[cfg(feature = "automation")]
    fn raw_input_hook(&mut self, _ctx: &egui::Context, raw_input: &mut egui::RawInput) {
        if let Some(automation) = &mut self.automation {
            automation.raw_input(raw_input);
        }
    }

    fn ui(&mut self, ui: &mut egui::Ui, frame: &mut eframe::Frame) {
        self.hud.begin_frame(frame.info().cpu_usage);
        let ctx = ui.ctx().clone();
        let ppp = ctx.pixels_per_point();
        if self.saved_display_scale != Some(ppp) {
            settings::save_display_scale(ppp);
            self.saved_display_scale = Some(ppp);
        }
        self.receive_opened(&ctx);
        if let Ok(engine) = &self.engine {
            self.tiles.begin_frame(engine, &ctx);
        }

        let (mut open_dialog, toggle_hud) = ctx.input_mut(|i| {
            let zoom_keys = [
                (Key::Equals, ZoomCommand::StepIn),
                (Key::Plus, ZoomCommand::StepIn),
                (Key::Minus, ZoomCommand::StepOut),
                (Key::Num0, ZoomCommand::Fit(Fit::Page)),
                (Key::Num1, ZoomCommand::Set(100.0)),
                (Key::Num2, ZoomCommand::Fit(Fit::Width)),
            ];
            for (key, command) in zoom_keys {
                if i.consume_key(Modifiers::COMMAND, key) {
                    self.zoom_command = Some(command);
                }
            }
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

        let mut tile_status = self.tiles.status();
        if let Ok(engine) = &self.engine {
            let workers = engine.status();
            tile_status.push_str(&format!(
                "\nworkers  {} ready, {} busy, {} tiles rendered",
                workers.helpers, workers.busy_helpers, workers.tiles_by_helpers
            ));
            if let Some(error) = &workers.helper_error {
                tile_status.push_str(&format!("\n         not available: {error}"));
            }
        }
        if let HudAction::RunScrollTest =
            self.hud
                .show(&ctx, self.auto_scroll.is_some(), &tile_status)
        {
            let speed = SCROLL_TEST_SPEED / self.camera.screen_per_pt();
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

        #[cfg(feature = "automation")]
        if let Some(automation) = &mut self.automation {
            let workers = self.engine.as_ref().map(|e| e.status()).unwrap_or_default();
            automation.end_frame(&ctx, frame.info().cpu_usage, self.tiles.stats(), &workers);
        }

        let animating = moving || self.auto_scroll.is_some();
        self.hud.end_frame(animating);
        if animating || self.tiles.is_busy() {
            ctx.request_repaint();
        }
    }
}

const FULL_UV: Rect = Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0));

/// One page's tiles at one scale, to draw into `page_rect` on screen.
struct PageTiles {
    doc: DocId,
    page: u32,
    size: PageSize,
    page_rect: Rect,
    viewport: Rect,
    ppp: f32,
    view_px: (f32, f32),
    scale: Scale,
    /// `Some` requests missing tiles with this base priority; `None` only draws cached ones.
    priority: Option<u32>,
}

/// Draws the cached tiles of a page at `scale`, stretched to the page's current size on
/// screen (1:1 when the zoom has settled), and requests missing ones. Returns how many
/// tiles on screen are still missing.
fn draw_page_tiles(tiles: &mut TileManager, painter: &Painter, p: PageTiles) -> usize {
    let page_px = page_px_size(p.size, p.scale);
    // Tile pixels per screen pixel: 1.0 once the zoom has settled.
    let k = page_px.0 as f32 / (p.page_rect.width() * p.ppp);
    let origin = (
        (p.page_rect.left() - p.viewport.left()) * p.ppp * k,
        (p.page_rect.top() - p.viewport.top()) * p.ppp * k,
    );
    let view = (p.view_px.0 * k, p.view_px.1 * k);
    let (cols, rows) = if p.priority.is_some() {
        // Also the screen above and below, to prefetch.
        visible_tiles(
            page_px,
            (origin.0, origin.1 + view.1),
            (view.0, view.1 * 3.0),
        )
    } else {
        visible_tiles(page_px, origin, view)
    };
    let to_screen = 1.0 / (k * p.ppp);
    let mut missing = 0;
    for ty in rows {
        for tx in cols.clone() {
            let Some(r) = tile_rect(page_px, tx, ty) else {
                continue;
            };
            let screen = Rect::from_min_size(
                p.page_rect.min + vec2(r.x as f32, r.y as f32) * to_screen,
                vec2(r.width as f32, r.height as f32) * to_screen,
            );
            let key = TileKey {
                doc: p.doc,
                page: p.page,
                scale: p.scale,
                tx,
                ty,
            };
            let texture = match p.priority {
                Some(base) => {
                    let dx = origin.0 + (r.x + r.width / 2) as f32 - view.0 / 2.0;
                    let dy = origin.1 + (r.y + r.height / 2) as f32 - view.1 / 2.0;
                    let tiles_away = ((dx.abs() + dy.abs()) / TILE_SIZE as f32) as u32;
                    tiles.get(key, base + tiles_away, Quality::Sharp)
                }
                None => tiles.peek(key),
            };
            let visible = screen.intersects(p.viewport);
            match texture {
                Some(texture) if visible => {
                    painter.image(texture.id(), screen, FULL_UV, Color32::WHITE);
                }
                None if visible => missing += 1,
                _ => {}
            }
        }
    }
    missing
}

/// Handles dragging the thumb and clicking the track of a scrollbar. Returns the thumb's
/// rectangle and whether it is hovered, for painting.
fn scrollbar(
    ui: &mut egui::Ui,
    bar: Rect,
    horizontal: bool,
    scroll: &mut SmoothScroll,
    max: f32,
    visible_fraction: f32,
) -> (Rect, bool) {
    let response = ui.interact(
        bar,
        ui.id().with(("scrollbar", horizontal)),
        Sense::click_and_drag(),
    );
    let length = if horizontal {
        bar.width()
    } else {
        bar.height()
    };
    let thumb_len = (length * visible_fraction).max(32.0).min(length);
    let track = length - thumb_len;
    if track > 0.0 && max > 0.0 {
        let along = |v: Vec2| if horizontal { v.x } else { v.y };
        if response.dragged() {
            scroll.jump_by(along(response.drag_delta()) * max / track);
        } else if response.clicked()
            && let Some(pointer) = response.interact_pointer_pos()
        {
            let start = along(pointer - bar.min) - thumb_len / 2.0;
            scroll.scroll_to((start / track).clamp(0.0, 1.0) * max);
        }
    }
    let offset = if max > 0.0 {
        track * (scroll.position() / max).clamp(0.0, 1.0)
    } else {
        0.0
    };
    let thumb = if horizontal {
        Rect::from_min_size(
            pos2(bar.left() + offset + 2.0, bar.top() + 2.0),
            vec2(thumb_len - 4.0, SCROLLBAR_WIDTH - 4.0),
        )
    } else {
        Rect::from_min_size(
            pos2(bar.left() + 2.0, bar.top() + offset + 2.0),
            vec2(SCROLLBAR_WIDTH - 4.0, thumb_len - 4.0),
        )
    };
    (thumb, response.hovered() || response.dragged())
}

fn paint_scrollbar(painter: &Painter, bar: Rect, (thumb, active): (Rect, bool)) {
    painter.rect_filled(bar, 0.0, Color32::from_black_alpha(40));
    let color = if active {
        Color32::from_gray(215)
    } else {
        Color32::from_gray(160)
    };
    painter.rect_filled(thumb, 4.0, color);
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
