//! Scripted scenarios for performance tests and CI smoke tests.
//!
//! Compiled only with the `automation` feature, so release builds for users do not contain
//! it. The `perf-runner` tool (crates/perf) starts the app with:
//!
//! - `KRAKEN_AUTOMATION`: the scenario name (`startup`, `scroll`, `zoom`, `idle`, `tour`,
//!   `soak`, `smoke`),
//! - `KRAKEN_PERF_REPORT`: where to write the JSON report,
//!
//! and measures CPU and memory of the process from outside while it runs.

use std::path::PathBuf;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use eframe::egui;
use pdf_view::Camera;
use serde_json::json;

use crate::tiles::TileStats;

/// Named moments during startup, in order, for the startup breakdown.
static MARKS: std::sync::Mutex<Vec<(&'static str, f64)>> = std::sync::Mutex::new(Vec::new());

/// Records that startup reached `name` now.
pub fn mark(name: &'static str) {
    if let Ok(mut marks) = MARKS.lock() {
        marks.push((name, unix_ms()));
    }
}

/// Waiting longer than this for the first page means the run failed.
const LOAD_TIMEOUT: Duration = Duration::from_secs(60);

#[derive(Clone, Copy, Debug)]
enum Action {
    /// Scroll down and back up at this many screen points per second.
    Scroll(f32),
    /// Zoom from 100% to 800%, hold, down to 50%, hold.
    ZoomSweep,
    /// Do nothing; the app should not draw at all.
    Idle,
    /// Scroll through the whole document once at this many screen points per second.
    Tour(f32),
    /// Wait until no tiles are rendering, including prefetched ones.
    Settle,
}

#[derive(Clone, Copy, Debug)]
struct Phase {
    name: &'static str,
    seconds: f32,
    action: Action,
}

fn scenario(name: &str) -> Option<Vec<Phase>> {
    let phase = |name, seconds, action| Phase {
        name,
        seconds,
        action,
    };
    Some(match name {
        "startup" => vec![],
        "scroll" => vec![phase("scroll", 8.0, Action::Scroll(2400.0))],
        "zoom" => vec![phase("zoom", 6.0, Action::ZoomSweep)],
        // Let prefetching finish first: that is real work, not idling.
        "idle" => vec![
            phase("settle", 20.0, Action::Settle),
            phase("idle", 6.0, Action::Idle),
        ],
        "tour" => vec![
            phase("tour", 120.0, Action::Tour(40_000.0)),
            phase("settle", 3.0, Action::Settle),
        ],
        // The zoom sweep five times in one process: memory must not keep growing.
        "soak" => ["soak-1", "soak-2", "soak-3", "soak-4", "soak-5"]
            .into_iter()
            .map(|name| phase(name, 6.0, Action::ZoomSweep))
            .collect(),
        "smoke" => vec![
            phase("scroll", 2.0, Action::Scroll(2400.0)),
            phase("zoom", 6.0, Action::ZoomSweep),
        ],
        _ => return None,
    })
}

pub struct Automation {
    scenario_name: String,
    report_path: Option<PathBuf>,
    phases: Vec<Phase>,
    started: Instant,
    opened_ms: Option<f64>,
    first_page_ms: Option<f64>,
    first_page_unix_ms: Option<f64>,
    /// Frames drawn from opening the document to the first sharp page.
    frames_to_first_page: u32,
    current: usize,
    phase_started: Option<Instant>,
    phase_log: Vec<serde_json::Value>,
    frames: Vec<serde_json::Value>,
    last_frame: Option<Instant>,
    scroll_direction: f32,
    failure: Option<String>,
    finished: bool,
}

/// What the app tells the automation each frame.
pub struct FrameState {
    pub document_open: bool,
    pub render_complete: bool,
    pub tiles_pending: bool,
    pub content: (f32, f32),
    pub view: (f32, f32),
    pub dt: f32,
}

impl Automation {
    pub fn from_env() -> Option<Self> {
        let name = std::env::var("KRAKEN_AUTOMATION").ok()?;
        mark("app_ready");
        let (phases, failure) = match scenario(&name) {
            Some(phases) => (phases, None),
            None => (vec![], Some(format!("unknown scenario {name}"))),
        };
        Some(Automation {
            scenario_name: name,
            report_path: std::env::var_os("KRAKEN_PERF_REPORT").map(PathBuf::from),
            phases,
            started: Instant::now(),
            opened_ms: None,
            first_page_ms: None,
            first_page_unix_ms: None,
            frames_to_first_page: 0,
            current: 0,
            phase_started: None,
            phase_log: Vec::new(),
            frames: Vec::new(),
            last_frame: None,
            scroll_direction: 1.0,
            failure,
            finished: false,
        })
    }

    fn since_start_ms(&self) -> f64 {
        self.started.elapsed().as_secs_f64() * 1000.0
    }

    /// Moves the camera for the current phase. Returns `true` if it wants another frame.
    pub fn drive(&mut self, camera: &mut Camera, state: &FrameState) -> bool {
        if self.finished || self.failure.is_some() {
            return true;
        }
        let now_ms = self.since_start_ms();
        if state.document_open && self.opened_ms.is_none() {
            self.opened_ms = Some(now_ms);
            mark("pdf_opened");
        }
        if self.first_page_ms.is_none() {
            if state.document_open {
                self.frames_to_first_page += 1;
            }
            if state.document_open && state.render_complete {
                self.first_page_ms = Some(now_ms);
                self.first_page_unix_ms = Some(unix_ms());
                mark("first_sharp_page");
            } else if self.started.elapsed() > LOAD_TIMEOUT {
                self.failure = Some("the first page did not finish rendering".into());
            }
            return true;
        }

        let Some(phase) = self.phases.get(self.current).copied() else {
            self.finished = true;
            return true;
        };
        let started = *self.phase_started.get_or_insert_with(|| {
            self.phase_log
                .push(json!({ "name": phase.name, "start_unix_ms": unix_ms() }));
            Instant::now()
        });
        let elapsed = started.elapsed().as_secs_f32();
        let s = camera.screen_per_pt();
        let (_, max_y) = camera.max_scroll(state.content, state.view);
        let center = (state.view.0 / 2.0, state.view.1 / 2.0);
        let mut done = elapsed >= phase.seconds;

        match phase.action {
            Action::Scroll(speed) => {
                camera
                    .y
                    .jump_by(self.scroll_direction * speed / s * state.dt);
                if camera.y.position() >= max_y {
                    self.scroll_direction = -1.0;
                } else if camera.y.position() <= 0.0 {
                    self.scroll_direction = 1.0;
                }
            }
            Action::ZoomSweep => {
                // 0-2 s: 100% -> 800%, 2-3 s: hold, 3-5 s: 800% -> 50%, 5-6 s: hold.
                let zoom = match elapsed {
                    t if t < 2.0 => 100.0 * 8f32.powf(t / 2.0),
                    t if t < 3.0 => 800.0,
                    t if t < 5.0 => 800.0 * (50.0f32 / 800.0).powf((t - 3.0) / 2.0),
                    _ => 50.0,
                };
                camera.zoom_around(zoom, center, state.content, state.view);
            }
            Action::Idle => {}
            Action::Tour(speed) => {
                camera.y.jump_by(speed / s * state.dt);
                done |= camera.y.position() >= max_y;
            }
            Action::Settle => done |= state.render_complete && !state.tiles_pending,
        }

        if done {
            if let Some(entry) = self.phase_log.last_mut() {
                entry["end_unix_ms"] = json!(unix_ms());
                entry["seconds"] = json!(elapsed);
            }
            self.current += 1;
            self.phase_started = None;
            return true;
        }
        // Idle must not draw; wake up once when it is over.
        !matches!(phase.action, Action::Idle)
    }

    /// Records the frame's timing, and when the scenario is over, writes the report and
    /// closes the window.
    pub fn end_frame(&mut self, ctx: &egui::Context, cpu_usage_s: Option<f32>, tiles: &TileStats) {
        let now = Instant::now();
        if let (Some(last), Some(phase)) = (
            self.last_frame,
            self.phase_started.and(self.phases.get(self.current)),
        ) {
            self.frames.push(json!({
                "phase": phase.name,
                "interval_ms": (now - last).as_secs_f64() * 1000.0,
                "cpu_ms": cpu_usage_s.unwrap_or(0.0) as f64 * 1000.0,
            }));
        }
        self.last_frame = Some(now);

        if let Some(phase) = self.phase_started.and(self.phases.get(self.current))
            && matches!(phase.action, Action::Idle)
        {
            let left = phase.seconds
                - self
                    .phase_started
                    .map_or(0.0, |s| s.elapsed().as_secs_f32());
            ctx.request_repaint_after(Duration::from_secs_f32(left.max(0.0) + 0.01));
            // Frames during idle are unexpected; the next one ends the phase.
            self.last_frame = None;
        }

        if self.finished || self.failure.is_some() {
            self.write_report(tiles);
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            // Stop writing again on the frames until the window closes.
            self.finished = true;
            self.report_path = None;
        }
    }

    fn write_report(&self, tiles: &TileStats) {
        let Some(path) = &self.report_path else {
            return;
        };
        let marks: Vec<serde_json::Value> = MARKS
            .lock()
            .map(|m| {
                m.iter()
                    .map(|(name, t)| json!({ "name": name, "unix_ms": t }))
                    .collect()
            })
            .unwrap_or_default();
        let mut render_ms: Vec<f32> = tiles.render_ms.iter().copied().collect();
        render_ms.sort_by(f32::total_cmp);
        let report = json!({
            "scenario": self.scenario_name,
            "ok": self.failure.is_none(),
            "failure": self.failure,
            "open_ms": self.opened_ms,
            "first_page_ms": self.first_page_ms,
            "first_page_unix_ms": self.first_page_unix_ms,
            "startup": {
                "marks": marks,
                "frames_to_first_page": self.frames_to_first_page,
            },
            "phases": self.phase_log,
            "frames": self.frames,
            "tiles": {
                "rendered": tiles.rendered,
                "render_ms": render_ms,
                "peak_cache_mb": tiles.peak_cache_bytes as f64 / (1024.0 * 1024.0),
                "peak_ready": tiles.peak_ready,
                "peak_ready_mb": tiles.peak_ready_bytes as f64 / (1024.0 * 1024.0),
                "discarded": tiles.discarded,
            },
        });
        if let Err(e) = std::fs::write(path, report.to_string()) {
            eprintln!(
                "could not write the performance report to {}: {e}",
                path.display()
            );
        }
    }
}

fn unix_ms() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0.0, |d| d.as_secs_f64() * 1000.0)
}
