//! Scripted scenarios for performance tests and CI smoke tests.
//!
//! Compiled only with the `automation` feature, so release builds for users do not contain
//! it. The `perf-runner` tool (crates/perf) starts the app with:
//!
//! - `KRAKEN_AUTOMATION`: the scenario name (`startup`, `scroll`, `zoom`, `idle`, `tour`,
//!   `soak`, `sharpen`, `wheel`, `spin`, `smoke`),
//! - `KRAKEN_PERF_REPORT`: where to write the JSON report,
//!
//! and measures CPU and memory of the process from outside while it runs.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use eframe::egui;
use pdf_view::Camera;
use serde_json::json;

use crate::tiles::TileStats;

/// Threads and the CPU milliseconds they used, busiest first.
type ThreadTimes = Vec<(String, f64)>;

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
    /// A 0.4 s zoom gesture to this zoom, then wait until the view is sharp again.
    ZoomTo(f32),
    /// Real Ctrl+mouse-wheel input: this many notches (negative zooms out) in one frame over
    /// the middle of the window, then wait until the view is sharp again.
    WheelNotches(i32),
    /// A fast wheel spin: `count` notches in this direction, one every [`SPIN_INTERVAL`]
    /// seconds, then wait until the view is sharp again.
    WheelSpin { notches: i32, count: u32 },
}

/// Time between notches in [`Action::WheelSpin`]: about 16 notches per second, a quick flick.
const SPIN_INTERVAL: f32 = 0.06;

/// Length of the zoom gesture in [`Action::ZoomTo`].
const GESTURE_SECONDS: f32 = 0.4;

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
        // Zoom gestures to different levels; each measures how long the view stays blurry
        // after the fingers stop.
        "sharpen" => [200.0, 400.0, 150.0, 300.0, 100.0, 250.0]
            .into_iter()
            .map(|zoom| phase("sharpen", 10.0, Action::ZoomTo(zoom)))
            .collect(),
        // Ctrl+wheel notches, sent as real input events: time from the notch to a sharp view.
        "wheel" => [1, 1, 2, -1, 2, -3, -1]
            .into_iter()
            .map(|notches| phase("wheel", 10.0, Action::WheelNotches(notches)))
            .collect(),
        // Fast Ctrl+wheel spins in and out: time from the last notch to a sharp view, and the
        // tiles rendered for zoom steps that were only passed through.
        "spin" => [1, -1, 1, -1]
            .into_iter()
            .map(|notches| phase("spin", 10.0, Action::WheelSpin { notches, count: 8 }))
            .collect(),
        "smoke" => vec![
            phase("scroll", 2.0, Action::Scroll(2400.0)),
            phase("zoom", 6.0, Action::ZoomSweep),
        ],
        _ => return None,
    })
}

pub struct Automation {
    /// The GPU drawing the window, see `gpu_description`.
    gpu: String,
    scenario_name: String,
    report_path: Option<PathBuf>,
    phases: Vec<Phase>,
    started: Instant,
    opened_ms: Option<f64>,
    first_page_ms: Option<f64>,
    first_page_unix_ms: Option<f64>,
    /// Frames drawn from opening the document to the first sharp page.
    frames_to_first_page: u32,
    gesture_start_zoom: Option<f32>,
    gesture_end: Option<Instant>,
    /// For each zoom gesture: milliseconds from the fingers stopping to a sharp view.
    sharpen_ms: Vec<f64>,
    /// Most render workers running at once, and their latest counters.
    peak_workers: usize,
    workers: pdf_engine::PoolStatus,
    /// CPU time per thread during the idle window, filled in by a measuring thread.
    idle_threads: Arc<Mutex<Option<ThreadTimes>>>,
    /// When the GPU finished the work of the frame that started the idle phase (Unix ms),
    /// filled in by the same thread.
    gpu_quiet_unix_ms: Arc<Mutex<Option<f64>>>,
    /// Wheel notches to send with the next frame's input.
    pending_wheel: Option<i32>,
    /// Notches sent so far in the current [`Action::WheelSpin`], and when the last one was.
    spin_sent: u32,
    spin_last: Option<Instant>,
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
    /// Tiles are requested for the zoom on screen (not waiting for a gesture to settle).
    pub zoom_settled: bool,
    pub content: (f32, f32),
    pub view: (f32, f32),
    pub dt: f32,
}

impl Automation {
    pub fn from_env(gpu: String) -> Option<Self> {
        let name = std::env::var("KRAKEN_AUTOMATION").ok()?;
        mark("app_ready");
        let (phases, failure) = match scenario(&name) {
            Some(phases) => (phases, None),
            None => (vec![], Some(format!("unknown scenario {name}"))),
        };
        Some(Automation {
            gpu,
            scenario_name: name,
            report_path: std::env::var_os("KRAKEN_PERF_REPORT").map(PathBuf::from),
            phases,
            started: Instant::now(),
            opened_ms: None,
            first_page_ms: None,
            first_page_unix_ms: None,
            frames_to_first_page: 0,
            gesture_start_zoom: None,
            gesture_end: None,
            sharpen_ms: Vec::new(),
            pending_wheel: None,
            idle_threads: Arc::default(),
            gpu_quiet_unix_ms: Arc::default(),
            peak_workers: 0,
            workers: pdf_engine::PoolStatus::default(),
            spin_sent: 0,
            spin_last: None,
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
            Action::ZoomTo(target) => {
                let start_zoom = *self.gesture_start_zoom.get_or_insert(camera.zoom());
                if elapsed < GESTURE_SECONDS {
                    let t = elapsed / GESTURE_SECONDS;
                    let zoom = start_zoom * (target / start_zoom).powf(t);
                    camera.zoom_around(zoom, center, state.content, state.view);
                } else {
                    let stopped = *self.gesture_end.get_or_insert_with(|| {
                        camera.zoom_around(target, center, state.content, state.view);
                        Instant::now()
                    });
                    if state.zoom_settled && state.render_complete {
                        self.sharpen_ms
                            .push(stopped.elapsed().as_secs_f64() * 1000.0);
                        done = true;
                    }
                }
            }
            Action::WheelNotches(notches) => {
                let start_zoom = *self.gesture_start_zoom.get_or_insert_with(|| {
                    self.pending_wheel = Some(notches);
                    camera.zoom()
                });
                let sent = *self.gesture_end.get_or_insert_with(Instant::now);
                if camera.zoom() != start_zoom && state.zoom_settled && state.render_complete {
                    self.sharpen_ms.push(sent.elapsed().as_secs_f64() * 1000.0);
                    done = true;
                }
            }
            Action::WheelSpin { notches, count } => {
                let due = self
                    .spin_last
                    .is_none_or(|last| last.elapsed().as_secs_f32() >= SPIN_INTERVAL);
                if self.spin_sent < count {
                    if due {
                        self.pending_wheel = Some(notches);
                        self.spin_sent += 1;
                        self.spin_last = Some(Instant::now());
                    }
                } else if self.pending_wheel.is_none()
                    && state.zoom_settled
                    && state.render_complete
                    && let Some(last) = self.spin_last
                {
                    self.sharpen_ms.push(last.elapsed().as_secs_f64() * 1000.0);
                    done = true;
                }
            }
        }

        if done {
            self.gesture_start_zoom = None;
            self.gesture_end = None;
            self.spin_sent = 0;
            self.spin_last = None;
            if let Some(entry) = self.phase_log.last_mut() {
                entry["end_unix_ms"] = json!(unix_ms());
                entry["seconds"] = json!(elapsed);
                if matches!(phase.action, Action::Idle)
                    && let Some(quiet) = self
                        .gpu_quiet_unix_ms
                        .lock()
                        .ok()
                        .and_then(|mut q| q.take())
                {
                    entry["quiet_from_unix_ms"] = json!(quiet);
                }
                if matches!(phase.action, Action::Idle)
                    && let Some(busy) = self.idle_threads.lock().ok().and_then(|mut r| r.take())
                {
                    entry["threads"] = json!(
                        busy.iter()
                            .map(|(name, ms)| json!({ "name": name, "cpu_ms": ms }))
                            .collect::<Vec<_>>()
                    );
                }
            }
            self.current += 1;
            self.phase_started = None;
            return true;
        }
        // Idle must not draw; wake up once when it is over.
        !matches!(phase.action, Action::Idle)
    }

    /// Adds scripted input events to the next frame.
    pub fn raw_input(&mut self, raw_input: &mut egui::RawInput) {
        let Some(notches) = self.pending_wheel.take() else {
            return;
        };
        let Some(screen) = raw_input.screen_rect else {
            self.pending_wheel = Some(notches);
            return;
        };
        raw_input
            .events
            .push(egui::Event::PointerMoved(screen.center()));
        raw_input.events.push(egui::Event::MouseWheel {
            unit: egui::MouseWheelUnit::Line,
            delta: egui::vec2(0.0, notches as f32),
            phase: egui::TouchPhase::Move,
            modifiers: egui::Modifiers::COMMAND,
        });
    }

    /// Records the frame's timing, and when the scenario is over, writes the report and
    /// closes the window.
    pub fn end_frame(
        &mut self,
        ctx: &egui::Context,
        cpu_usage_s: Option<f32>,
        tiles: &TileStats,
        workers: &pdf_engine::PoolStatus,
        gpu: Option<&eframe::wgpu::Device>,
    ) {
        self.peak_workers = self.peak_workers.max(workers.helpers);
        self.workers = workers.clone();
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
            // The frame that started the phase is still being drawn: it is submitted to
            // the GPU after this call, and with software rendering (WARP on the CI runners)
            // the GPU's work runs on CPU threads for up to a second. Idling starts once the
            // GPU has finished it.
            if let Some(entry) = self.phase_log.last_mut()
                && entry.get("quiet_from_unix_ms").is_none()
            {
                entry["quiet_from_unix_ms"] = json!(unix_ms());
                // Which threads use CPU while idle. perf-runner measures from 1 s after
                // the GPU went quiet to 0.5 s before the phase ends; the readings are taken
                // just outside that window (0.75 s and 0.25 s) so reading the thread times,
                // which costs a little CPU itself, does not count as idle CPU.
                let phase_end = Instant::now() + Duration::from_secs_f32(left.max(0.0));
                let device = gpu.cloned();
                let quiet = Arc::clone(&self.gpu_quiet_unix_ms);
                let result = Arc::clone(&self.idle_threads);
                std::thread::spawn(move || {
                    if let Some(device) = device {
                        // Give the frame time to be submitted, then wait for the GPU.
                        std::thread::sleep(Duration::from_millis(100));
                        let _ = device.poll(eframe::wgpu::PollType::wait_indefinitely());
                        if let Ok(mut quiet) = quiet.lock() {
                            *quiet = Some(unix_ms());
                        }
                    }
                    std::thread::sleep(Duration::from_millis(750));
                    let before = crate::threads::cpu_times();
                    let until = phase_end.checked_sub(Duration::from_millis(250));
                    let window = until.map_or(Duration::ZERO, |u| {
                        u.saturating_duration_since(Instant::now())
                    });
                    std::thread::sleep(window.max(Duration::from_millis(250)));
                    let busy = crate::threads::busy_between(&before, &crate::threads::cpu_times());
                    if let Ok(mut result) = result.lock() {
                        *result = Some(busy);
                    }
                });
            }
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
            "gpu": self.gpu,
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
            "sharpen_ms": self.sharpen_ms,
            "frames": self.frames,
            "tiles": {
                "rendered": tiles.rendered,
                "render_ms": render_ms,
                "peak_cache_mb": tiles.peak_cache_bytes as f64 / (1024.0 * 1024.0),
                "peak_ready": tiles.peak_ready,
                "peak_ready_mb": tiles.peak_ready_bytes as f64 / (1024.0 * 1024.0),
                "discarded": tiles.discarded,
                "cancelled": tiles.cancelled,
                "duplicates": tiles.duplicates,
            },
            "workers": {
                "peak": self.peak_workers,
                "tiles": self.workers.tiles_by_helpers,
                "cancelled": self.workers.tiles_cancelled,
                "crashes": self.workers.helper_crashes,
                "error": self.workers.helper_error,
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
