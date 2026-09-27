//! perf-runner: measures the engine and the real app process against `perf/budgets.toml`.
//!
//! ```text
//! cargo run --release -p perf -- --suite all
//! ```
//!
//! The app suite starts the app once per scenario, drives it through its `automation`
//! feature, samples the process's CPU and memory from outside every 250 ms, and combines
//! that with the frame timing the app reports.

use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use clap::{Parser, ValueEnum};
use pdf_engine::geometry::{page_px_size, tile_grid};
use pdf_engine::{Engine, EngineConfig, Quality, Scale, TileKey, TileRequest};
use perf::budgets::Budgets;
use perf::fixtures::{self, Fixtures};
use perf::report::Report;
use perf::stats::{max, mean, missed_frames_pct, percentile};
use serde_json::Value;
use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, System};

#[derive(Parser)]
#[command(about = "Measure Kraken PDF against its performance budgets")]
struct Args {
    #[arg(long, value_enum, default_value_t = Suite::All)]
    suite: Suite,

    /// `software` on machines without a GPU (CI): frame timing is then reported but not
    /// enforced.
    #[arg(long, value_enum, default_value_t = Gpu::Real)]
    gpu: Gpu,

    /// App scenarios to run.
    #[arg(
        long,
        value_delimiter = ',',
        default_value = "startup,scroll,zoom,sharpen,wheel,spin,idle,tour,soak"
    )]
    scenarios: Vec<String>,

    /// The app, built with `--features automation`. By default perf-runner builds it into
    /// target/perf-app.
    #[arg(long)]
    app: Option<PathBuf>,

    #[arg(long)]
    budgets: Option<PathBuf>,

    /// PDF for the app scenarios. Defaults to the generated 500-page text fixture. Budgets
    /// are written for that fixture, so with another PDF read the numbers, not PASS/FAIL.
    #[arg(long)]
    pdf: Option<PathBuf>,

    /// Where reports and fixtures go. Defaults to target/perf-report.
    #[arg(long)]
    out: Option<PathBuf>,
}

#[derive(Clone, Copy, PartialEq, ValueEnum)]
enum Suite {
    All,
    Engine,
    App,
}

#[derive(Clone, Copy, PartialEq, ValueEnum)]
enum Gpu {
    Real,
    Software,
}

const APP_TIMEOUT: Duration = Duration::from_secs(300);
const SAMPLE_EVERY: Duration = Duration::from_millis(250);
const MEMORY_EVERY: Duration = Duration::from_millis(10);
/// Idle CPU is measured after this grace period: the frame that ends the previous activity
/// is still being drawn when the idle phase starts.
const IDLE_GRACE_MS: f64 = 1000.0;
/// ...and stops this long before the phase ends: the test wakes the app to end the idle
/// phase, and that frame is the test's own doing (on Windows CI it showed up as a single
/// 213% sample after 19 samples of 0%).
const IDLE_TAIL_MS: f64 = 500.0;

struct Ctx<'a> {
    budgets: &'a Budgets,
    real_gpu: bool,
    report: Report,
}

impl Ctx<'_> {
    /// Notes once which GPU the app drew with, and flags software rendering on a run that
    /// is supposed to use a real GPU (its frame timing would be meaningless).
    fn note_gpu(&mut self, report: &Value) {
        let Some(gpu) = report["gpu"].as_str() else {
            return;
        };
        let note = format!("App drew with: {gpu}");
        if self.report.notes.contains(&note) {
            return;
        }
        self.report.notes.push(note);
        if self.real_gpu && gpu.contains("SOFTWARE") {
            self.report.notes.push(
                "WARNING: the app ran without GPU acceleration although --gpu real was \
                 given; frame timing reflects software rendering."
                    .into(),
            );
        }
    }

    fn add(&mut self, name: &str, value: f64, unit: &'static str) {
        self.report
            .add(self.budgets, self.real_gpu, name, value, unit);
    }
}

fn main() -> Result<()> {
    let args = Args::parse();
    if let Some(pdf) = &args.pdf
        && !pdf.is_file()
    {
        bail!(
            "--pdf: no such file: {} (put the path in quotes if it contains spaces)",
            pdf.display()
        );
    }
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("..").join("..");
    let out = args
        .out
        .clone()
        .unwrap_or_else(|| root.join("target").join("perf-report"));
    let budgets = Budgets::load(
        &args
            .budgets
            .clone()
            .unwrap_or_else(|| root.join("perf").join("budgets.toml")),
    )?;
    println!("Generating fixtures...");
    let fixtures = fixtures::ensure(&out.join("fixtures"))?;

    let mut ctx = Ctx {
        budgets: &budgets,
        real_gpu: args.gpu == Gpu::Real,
        report: Report::default(),
    };
    if !ctx.real_gpu {
        ctx.report
            .notes
            .push("Run without a real GPU: frame timing budgets are not enforced.".into());
    }

    if matches!(args.suite, Suite::All | Suite::Engine) {
        println!("Engine suite...");
        if let Err(e) = engine_suite(&fixtures, &mut ctx) {
            ctx.report.errors.push(format!("engine suite: {e:#}"));
        }
    }
    if matches!(args.suite, Suite::All | Suite::App) {
        let app = match args.app.clone() {
            Some(app) => app,
            None => build_app(&root)?,
        };
        if !app.is_file() {
            bail!("{} not found", app.display());
        }
        let pdf = args.pdf.clone().unwrap_or_else(|| fixtures.text.clone());
        if args.pdf.is_some() {
            ctx.report.notes.push(format!(
                "App scenarios ran on {} instead of the 500-page fixture; budgets assume the fixture.",
                pdf.display()
            ));
        }
        for scenario in &args.scenarios {
            println!("App scenario '{scenario}'...");
            if let Err(e) = app_scenario(&app, scenario, &pdf, &out, &mut ctx) {
                ctx.report
                    .errors
                    .push(format!("app scenario {scenario}: {e:#}"));
            }
        }
    }

    ctx.report.write(&out)?;
    println!("\n{}", ctx.report.markdown());
    println!("Reports: {}", out.display());
    if ctx.report.failed() {
        std::process::exit(1);
    }
    Ok(())
}

// --- Engine ------------------------------------------------------------------------------

fn engine_suite(fixtures: &Fixtures, ctx: &mut Ctx) -> Result<()> {
    let engine = Engine::start(EngineConfig {
        page_cache: 16,
        ..EngineConfig::default()
    })?;

    let mut opens = Vec::new();
    for _ in 0..5 {
        let started = Instant::now();
        let doc = engine.open(&fixtures.text, None)?;
        opens.push(ms(started.elapsed()));
        engine.close(doc.id);
    }
    ctx.add("engine.open_500_pages_ms", percentile(&opens, 0.5), "ms");

    // 150% zoom on a 150% scaled display: a typical laptop reading setup.
    let scale = Scale::from_zoom(150.0, 1.5);
    for (kind, path, pages) in [
        ("text", &fixtures.text, 30),
        ("vector", &fixtures.vector, fixtures::VECTOR_PAGES),
        ("image", &fixtures.images, fixtures::IMAGE_PAGES),
    ] {
        let doc = engine.open(path, None)?;
        let mut tile_ms = Vec::new();
        let mut page_ms = Vec::new();
        let started = Instant::now();
        for page in 0..pages as u32 {
            let page_started = Instant::now();
            let (cols, rows) = tile_grid(page_px_size(doc.page_sizes[page as usize], scale));
            for ty in 0..rows {
                for tx in 0..cols {
                    engine.request_tile(TileRequest {
                        key: TileKey {
                            doc: doc.id,
                            page,
                            scale,
                            tx,
                            ty,
                        },
                        generation: 0,
                        priority: ty * cols + tx,
                        quality: Quality::Final,
                    });
                }
            }
            for _ in 0..cols * rows {
                let result = engine.results().recv_timeout(Duration::from_secs(60))?;
                tile_ms.push(ms(result.tile?.render_time));
            }
            page_ms.push(ms(page_started.elapsed()));
        }
        let pages_per_s = pages as f64 / started.elapsed().as_secs_f64();
        ctx.add(
            &format!("engine.{kind}.tile_p50_ms"),
            percentile(&tile_ms, 0.5),
            "ms",
        );
        ctx.add(
            &format!("engine.{kind}.tile_p95_ms"),
            percentile(&tile_ms, 0.95),
            "ms",
        );
        ctx.add(&format!("engine.{kind}.page_max_ms"), max(&page_ms), "ms");
        ctx.add(
            &format!("engine.{kind}.pages_per_s"),
            pages_per_s,
            "pages/s",
        );
        engine.close(doc.id);
    }
    Ok(())
}

// --- App ---------------------------------------------------------------------------------

struct Sample {
    unix_ms: f64,
    /// Percent of one CPU core.
    cpu_pct: f64,
    rss_mb: f64,
}

/// Builds the app with the `automation` feature into its own target folder, so a normal
/// `cargo run -p kraken-pdf` (which rebuilds target/release/kraken-pdf without the feature)
/// can never replace the binary being measured.
fn build_app(root: &Path) -> Result<PathBuf> {
    let target_dir = root.join("target").join("perf-app");
    println!("Building the app with automation hooks...");
    // Set by cargo when perf-runner itself runs through `cargo run`.
    let cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
    let status = Command::new(cargo)
        .current_dir(root)
        .args([
            "build",
            "--release",
            "-p",
            "kraken-pdf",
            "--features",
            "automation",
        ])
        .arg("--target-dir")
        .arg(&target_dir)
        .status()
        .context("running cargo build")?;
    if !status.success() {
        bail!("building the app failed");
    }
    Ok(target_dir
        .join("release")
        .join(format!("kraken-pdf{}", std::env::consts::EXE_SUFFIX)))
}

/// Starts the app for one scenario and waits for it to finish. Returns when it was
/// started, its report, and the CPU/memory samples taken meanwhile.
fn run_app(
    app: &Path,
    scenario: &str,
    pdf: &Path,
    report_path: &Path,
) -> Result<(f64, Value, Vec<Sample>)> {
    let _ = std::fs::remove_file(report_path);
    let spawned_unix_ms = unix_ms();
    let child = Command::new(app)
        .arg(pdf)
        .env("KRAKEN_AUTOMATION", scenario)
        .env("KRAKEN_PERF_REPORT", report_path)
        .spawn()
        .with_context(|| format!("starting {}", app.display()))?;
    let samples = sample_until_exit(child)?;

    let text = std::fs::read_to_string(report_path).context(
        "the app exited without writing its report (was it built with \
             `--features automation`?)",
    )?;
    let report: Value = serde_json::from_str(&text)?;
    if report["ok"] != Value::Bool(true) {
        bail!("{}", report["failure"]);
    }
    Ok((spawned_unix_ms, report, samples))
}

/// How many times the startup scenario runs. The first run is "cold": a freshly built
/// program is scanned by antivirus software and its files are not in the disk cache yet.
/// The later runs show what users see on every start after the first.
const STARTUP_RUNS: usize = 4;

/// Runs the startup scenario several times and reports where the time goes: the steps
/// between the moments the app marks (process start -> main -> window and GPU -> ...).
fn startup_scenario(app: &Path, pdf: &Path, out: &Path, ctx: &mut Ctx) -> Result<()> {
    let mut totals = Vec::new();
    let mut steps: Vec<(String, Vec<f64>)> = Vec::new();
    let mut frames = Vec::new();
    let mut rss = Vec::new();
    for run in 0..STARTUP_RUNS {
        let path = out.join(format!("app-startup-{}.json", run + 1));
        let (spawned, report, samples) = run_app(app, "startup", pdf, &path)?;
        ctx.note_gpu(&report);
        rss.extend(samples.iter().map(|s| s.rss_mb));
        let first_page = report["first_page_unix_ms"].as_f64().unwrap_or(f64::NAN);
        totals.push(first_page - spawned);
        if run == 0 {
            continue; // the cold run only counts as a total
        }
        frames.push(
            report["startup"]["frames_to_first_page"]
                .as_f64()
                .unwrap_or(f64::NAN),
        );
        let mut previous = spawned;
        for mark in report["startup"]["marks"].as_array().into_iter().flatten() {
            let name = mark["name"].as_str().unwrap_or("?").to_owned();
            let at = mark["unix_ms"].as_f64().unwrap_or(f64::NAN);
            match steps.iter_mut().find(|(n, _)| *n == name) {
                Some((_, values)) => values.push(at - previous),
                None => steps.push((name, vec![at - previous])),
            }
            previous = at;
        }
    }
    let warm = &totals[1..];
    ctx.add("app.startup.cold_first_page_ms", totals[0], "ms");
    ctx.add("app.startup.first_page_ms", percentile(warm, 0.5), "ms");
    for (i, (name, values)) in steps.iter().enumerate() {
        // Named after the moment each step ends; numbered so they sort in order.
        ctx.add(
            &format!("app.startup.step{}_until_{name}_ms", i + 1),
            percentile(values, 0.5),
            "ms",
        );
    }
    ctx.add(
        "app.startup.frames_to_first_page",
        percentile(&frames, 0.5),
        "frames",
    );
    ctx.add("app.startup.peak_rss_mb", max(&rss), "MB");
    ctx.report.notes.push(format!(
        "Startup runs to first sharp page (ms), the first one cold: {}. Steps are medians of \
         the warm runs.",
        totals
            .iter()
            .map(|t| format!("{t:.0}"))
            .collect::<Vec<_>>()
            .join(", ")
    ));
    Ok(())
}

fn app_scenario(app: &Path, scenario: &str, pdf: &Path, out: &Path, ctx: &mut Ctx) -> Result<()> {
    std::fs::create_dir_all(out)?;
    if scenario == "startup" {
        return startup_scenario(app, pdf, out, ctx);
    }
    let report_path = out.join(format!("app-{scenario}.json"));
    let (_, report, samples) = run_app(app, scenario, pdf, &report_path)?;
    ctx.note_gpu(&report);
    let rss: Vec<f64> = samples.iter().map(|s| s.rss_mb).collect();
    let mut soak_peaks = Vec::new();

    for phase in report["phases"].as_array().into_iter().flatten() {
        let name = phase["name"].as_str().unwrap_or("?");
        let start = phase["start_unix_ms"].as_f64().unwrap_or(0.0);
        let end = phase["end_unix_ms"].as_f64().unwrap_or(f64::MAX);
        // A CPU sample averages the time since the previous sample, so only samples whose
        // whole interval lies inside the phase belong to it.
        let interval = SAMPLE_EVERY.as_secs_f64() * 1000.0;
        let (from, to) = if name == "idle" {
            (start + IDLE_GRACE_MS, end - IDLE_TAIL_MS)
        } else {
            (start, end)
        };
        let in_phase: Vec<&Sample> = samples
            .iter()
            .filter(|s| s.unix_ms - interval >= from && s.unix_ms <= to)
            .collect();
        if std::env::var_os("PERF_DEBUG").is_some() {
            let cpu: Vec<String> = in_phase
                .iter()
                .map(|s| format!("{:.1}", s.cpu_pct))
                .collect();
            eprintln!("{scenario}/{name}: cpu samples {}", cpu.join(" "));
        }
        let cpu: Vec<f64> = in_phase.iter().map(|s| s.cpu_pct).collect();
        let phase_rss: Vec<f64> = in_phase.iter().map(|s| s.rss_mb).collect();
        let frames: Vec<&Value> = report["frames"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|f| f["phase"] == name)
            .collect();
        let intervals: Vec<f64> = frames
            .iter()
            .filter_map(|f| f["interval_ms"].as_f64())
            .collect();
        let ui_cpu: Vec<f64> = frames.iter().filter_map(|f| f["cpu_ms"].as_f64()).collect();
        let key = |metric: &str| format!("app.{scenario}.{metric}");

        match name {
            "scroll" | "zoom" => {
                // With vsync the app draws at the monitor's refresh rate, so frame times are
                // judged against the display's own frame time, not a fixed number.
                let normal_frame = percentile(&intervals, 0.5);
                ctx.add(&key("display_hz"), 1000.0 / normal_frame.max(1e-9), "Hz");
                ctx.add(&key("fps"), 1000.0 / mean(&intervals).max(1e-9), "fps");
                ctx.add(&key("frame_p99_ms"), percentile(&intervals, 0.99), "ms");
                ctx.add(
                    &key("frame_p99_vs_normal"),
                    percentile(&intervals, 0.99) / normal_frame.max(1e-9),
                    "x",
                );
                ctx.add(
                    &key("missed_frames_pct"),
                    missed_frames_pct(&intervals),
                    "%",
                );
                ctx.add(&key("ui_cpu_p99_ms"), percentile(&ui_cpu, 0.99), "ms");
                ctx.add(&key("process_cpu_pct"), mean(&cpu), "% core");
                ctx.add(&key("peak_rss_mb"), max(&phase_rss), "MB");
            }
            "idle" => {
                // Kept in the report so a failure shows whether CPU use was a spike or
                // constant polling.
                let list: Vec<String> = cpu.iter().map(|c| format!("{c:.1}")).collect();
                ctx.report.notes.push(format!(
                    "Idle CPU samples every {} ms (% of a core): {}",
                    SAMPLE_EVERY.as_millis(),
                    list.join(" ")
                ));
                ctx.add(&key("process_cpu_pct"), mean(&cpu), "% core");
                ctx.add(&key("frames"), frames.len() as f64, "frames");
            }
            "tour" => {
                ctx.add(
                    &key("seconds"),
                    phase["seconds"].as_f64().unwrap_or(0.0),
                    "s",
                );
                ctx.add(&key("peak_rss_mb"), max(&rss), "MB");
                ctx.add(
                    &key("tile_cache_peak_mb"),
                    report["tiles"]["peak_cache_mb"].as_f64().unwrap_or(0.0),
                    "MB",
                );
                ctx.add(
                    &key("tiles_duplicated"),
                    report["tiles"]["duplicates"].as_f64().unwrap_or(0.0),
                    "tiles",
                );
                let render: Vec<f64> = report["tiles"]["render_ms"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(Value::as_f64)
                    .collect();
                ctx.add(&key("tile_p95_ms"), percentile(&render, 0.95), "ms");
            }
            soak if soak.starts_with("soak-") => soak_peaks.push(max(&phase_rss)),
            _ => {}
        }
    }
    if matches!(scenario, "sharpen" | "wheel" | "spin") {
        let times: Vec<f64> = report["sharpen_ms"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_f64)
            .collect();
        let (what, key) = match scenario {
            "sharpen" => ("Zoom stop to sharp (ms), one per gesture", "after_zoom"),
            "wheel" => (
                "Ctrl+wheel notch to sharp (ms), one per notch",
                "after_notch",
            ),
            _ => (
                "Last notch of a spin to sharp (ms), one per spin",
                "after_spin",
            ),
        };
        ctx.report.notes.push(format!(
            "{what}: {}",
            times
                .iter()
                .map(|t| format!("{t:.0}"))
                .collect::<Vec<_>>()
                .join(", ")
        ));
        ctx.add(
            &format!("app.{scenario}.{key}_p50_ms"),
            percentile(&times, 0.5),
            "ms",
        );
        ctx.add(&format!("app.{scenario}.{key}_max_ms"), max(&times), "ms");

        // The cost of zooming: work done from the first gesture to the last sharp view.
        let phases = report["phases"]
            .as_array()
            .map(Vec::as_slice)
            .unwrap_or(&[]);
        let from = phases.first().and_then(|p| p["start_unix_ms"].as_f64());
        let to = phases.last().and_then(|p| p["end_unix_ms"].as_f64());
        if let (Some(from), Some(to)) = (from, to) {
            let interval = SAMPLE_EVERY.as_secs_f64() * 1000.0;
            let cpu_s: f64 = samples
                .iter()
                .filter(|s| s.unix_ms > from && s.unix_ms - interval < to + interval)
                .map(|s| s.cpu_pct / 100.0 * SAMPLE_EVERY.as_secs_f64())
                .sum();
            ctx.add(&format!("app.{scenario}.cpu_seconds"), cpu_s, "s");
        }
        let tiles = &report["tiles"];
        ctx.add(
            &format!("app.{scenario}.tiles_rendered"),
            tiles["rendered"].as_f64().unwrap_or(0.0),
            "tiles",
        );
        ctx.add(
            &format!("app.{scenario}.tiles_discarded"),
            tiles["discarded"].as_f64().unwrap_or(0.0),
            "tiles",
        );
        ctx.add(
            &format!("app.{scenario}.tiles_duplicated"),
            tiles["duplicates"].as_f64().unwrap_or(0.0),
            "tiles",
        );
        ctx.add(
            &format!("app.{scenario}.tiles_cancelled"),
            tiles["cancelled"].as_f64().unwrap_or(0.0),
            "tiles",
        );
        ctx.add(
            &format!("app.{scenario}.tile_cache_peak_mb"),
            tiles["peak_cache_mb"].as_f64().unwrap_or(0.0),
            "MB",
        );
        ctx.add(&format!("app.{scenario}.peak_rss_mb"), max(&rss), "MB");
    }
    if let (Some(first), Some(last)) = (soak_peaks.first(), soak_peaks.last()) {
        let peaks: Vec<String> = soak_peaks.iter().map(|p| format!("{p:.0}")).collect();
        ctx.report.notes.push(format!(
            "Soak: peak memory per zoom sweep (MB): {}",
            peaks.join(", ")
        ));
        ctx.add("app.soak.peak_rss_mb", max(&soak_peaks), "MB");
        // Growth after the first round (which fills caches) points to a leak.
        let second = soak_peaks.get(1).unwrap_or(first);
        ctx.add("app.soak.growth_after_round_2_mb", last - second, "MB");
    }
    Ok(())
}

/// Samples the child's CPU and memory until it exits. CPU use is averaged over
/// [`SAMPLE_EVERY`]; memory is read every [`MEMORY_EVERY`] and each sample keeps the peak
/// of its interval, so a short run (a fast startup) still has its peak recorded.
fn sample_until_exit(mut child: Child) -> Result<Vec<Sample>> {
    let pid = Pid::from_u32(child.id());
    let mut system = System::new();
    let full = ProcessRefreshKind::nothing().with_cpu().with_memory();
    let memory_only = ProcessRefreshKind::nothing().with_memory();
    let started = Instant::now();
    let mut next_sample = started + SAMPLE_EVERY;
    let mut peak_mb = 0.0f64;
    let mut samples = Vec::new();
    loop {
        if child.try_wait()?.is_some() {
            if peak_mb > 0.0 {
                // The last, partial interval: its memory peak counts, its CPU is unknown.
                samples.push(Sample {
                    unix_ms: unix_ms(),
                    cpu_pct: 0.0,
                    rss_mb: peak_mb,
                });
            }
            return Ok(samples);
        }
        if started.elapsed() > APP_TIMEOUT {
            let _ = child.kill();
            bail!("timed out after {} s", APP_TIMEOUT.as_secs());
        }
        let now = Instant::now();
        let full_sample = now >= next_sample;
        let kind = if full_sample { full } else { memory_only };
        system.refresh_processes_specifics(ProcessesToUpdate::Some(&[pid]), true, kind);
        if let Some(process) = system.process(pid) {
            peak_mb = peak_mb.max(process.memory() as f64 / (1024.0 * 1024.0));
            if full_sample {
                samples.push(Sample {
                    unix_ms: unix_ms(),
                    cpu_pct: process.cpu_usage() as f64,
                    rss_mb: peak_mb,
                });
                peak_mb = 0.0;
            }
        }
        if full_sample {
            next_sample += SAMPLE_EVERY;
        }
        std::thread::sleep(MEMORY_EVERY);
    }
}

fn ms(d: Duration) -> f64 {
    d.as_secs_f64() * 1000.0
}

fn unix_ms() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0.0, |d| d.as_secs_f64() * 1000.0)
}
