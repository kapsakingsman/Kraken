//! On-screen frame timing display (toggle with F3).

use std::time::Instant;

use eframe::egui::{self, Align2, Color32, RichText};
use pdf_view::stats::{CPU_BUDGET_MS, MIN_FRAMES};
use pdf_view::{FrameSample, FrameStats, FrameSummary};

/// About four seconds of frames at 144 Hz.
const LIVE_FRAMES: usize = 600;
/// Enough for the whole scroll test even on a 1000 Hz display.
const TEST_FRAMES: usize = 20_000;

pub struct Hud {
    pub visible: bool,
    /// The GPU drawing the window, see `gpu_description`.
    gpu: String,
    live: FrameStats,
    test: Option<FrameStats>,
    last_test: Option<FrameSummary>,
    last_frame_start: Option<Instant>,
    previous_frame_animated: bool,
}

pub enum HudAction {
    None,
    RunScrollTest,
}

impl Hud {
    pub fn new(gpu: String) -> Self {
        Hud {
            visible: true,
            gpu,
            live: FrameStats::new(LIVE_FRAMES),
            test: None,
            last_test: None,
            last_frame_start: None,
            previous_frame_animated: false,
        }
    }

    /// Call at the start of every frame. Only frames that follow an animated frame are
    /// measured: while idle the app does not draw at all, so those gaps are not stutters.
    pub fn begin_frame(&mut self, cpu_usage_s: Option<f32>) {
        let now = Instant::now();
        if let (Some(last), true) = (self.last_frame_start, self.previous_frame_animated) {
            let sample = FrameSample {
                interval_ms: (now - last).as_secs_f32() * 1000.0,
                cpu_ms: cpu_usage_s.unwrap_or(0.0) * 1000.0,
            };
            self.live.record(sample);
            if let Some(test) = &mut self.test {
                test.record(sample);
            }
        }
        self.last_frame_start = Some(now);
    }

    pub fn end_frame(&mut self, animated: bool) {
        self.previous_frame_animated = animated;
    }

    pub fn start_test(&mut self) {
        self.test = Some(FrameStats::new(TEST_FRAMES));
        self.last_test = None;
    }

    pub fn finish_test(&mut self) {
        self.last_test = self.test.take().and_then(|t| t.summary());
    }

    pub fn show(&self, ctx: &egui::Context, test_running: bool, tiles: &str) -> HudAction {
        if !self.visible {
            return HudAction::None;
        }
        let mut action = HudAction::None;
        egui::Area::new(egui::Id::new("frame_hud"))
            .anchor(Align2::RIGHT_TOP, [-14.0, 44.0])
            .interactable(true)
            .show(ctx, |ui| {
                egui::Frame::popup(ui.style())
                    .fill(Color32::from_black_alpha(210))
                    .show(ui, |ui| {
                        ui.set_width(340.0);
                        ui.style_mut().wrap_mode = Some(egui::TextWrapMode::Extend);
                        ui.label(RichText::new("Frame timing  (F3 hides)").strong());
                        match self.live.summary() {
                            Some(s) if s.frames >= 10 => summary_lines(ui, &s),
                            _ => {
                                ui.label("Scroll to measure. Only frames drawn while");
                                ui.label("something moves are counted.");
                            }
                        }
                        ui.label(RichText::new(tiles).monospace());
                        ui.label(RichText::new(format!("gpu      {}", self.gpu)).monospace());
                        ui.separator();
                        if test_running {
                            ui.label("Scroll test running...");
                        } else if ui.button("Run 8-second scroll test").clicked() {
                            action = HudAction::RunScrollTest;
                        }
                        if let Some(result) = &self.last_test {
                            test_result(ui, result);
                        }
                    });
            });
        action
    }
}

fn summary_lines(ui: &mut egui::Ui, s: &FrameSummary) {
    let mono = |text: String| RichText::new(text).monospace();
    ui.label(mono(format!(
        "display  {:>5.0} Hz   fps {:>6.1}",
        s.refresh_hz, s.fps
    )));
    ui.label(mono(format!(
        "frame    p50 {:.2}  p99 {:.2}  max {:.1} ms",
        s.interval_p50_ms, s.interval_p99_ms, s.interval_max_ms
    )));
    let missed = format!(
        "missed   {} of {} ({:.1}%)",
        s.missed_frames,
        s.frames,
        s.missed_percent()
    );
    let color = if s.missed_percent() <= 1.0 {
        Color32::LIGHT_GREEN
    } else {
        Color32::from_rgb(255, 140, 120)
    };
    ui.label(mono(missed).color(color));
    ui.label(mono(format!(
        "UI CPU   avg {:.2}  p99 {:.2} ms (< {CPU_BUDGET_MS})",
        s.cpu_avg_ms, s.cpu_p99_ms
    )));
}

fn test_result(ui: &mut egui::Ui, s: &FrameSummary) {
    ui.add_space(4.0);
    let (verdict, color) = if s.is_smooth() {
        ("PASS", Color32::LIGHT_GREEN)
    } else {
        ("FAIL", Color32::from_rgb(255, 140, 120))
    };
    ui.label(
        RichText::new(format!("Last test: {verdict}"))
            .strong()
            .color(color),
    );
    summary_lines(ui, s);
    if s.frames < MIN_FRAMES {
        ui.label(format!("Only {} frames measured.", s.frames));
    }
    ui.add(
        egui::Label::new(format!(
            "Pass mark: under 1% missed frames and UI CPU p99 under {CPU_BUDGET_MS} ms \
             (frame budget at {:.0} Hz is {:.1} ms).",
            s.refresh_hz,
            s.frame_budget_ms()
        ))
        .wrap(),
    );
}
