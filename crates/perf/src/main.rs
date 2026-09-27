//! perf-runner: measures the engine and the real app process against `perf/budgets.toml`.
//!
//! ```text
//! cargo build --release -p kraken-pdf --features automation
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
use pdf_engine::{Engine, EngineConfig, Scale, TileKey, TileRequest};
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
        default_value = "startup,scroll,zoom,idle,tour,soak"
    )]
    scenarios: Vec<String>,

    /// The app, built with `--features automation`. Defaults to target/release/kraken-pdf.
    #[arg(long)]
    app: Option<PathBuf>,

    #[arg(long)]
    budgets: Option<PathBuf>,

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
/// Idle CPU is measured after this grace period: the frame that ends the previous activity
/// is still being drawn when the idle phase starts.
const IDLE_GRACE_MS: f64 = 1000.0;

struct Ctx<'a> {
    budgets: &'a Budgets,
    real_gpu: bool,
    report: Report,
}

impl Ctx<'_> {
    fn add(&mut self, name: &str, value: f64, unit: &'static str) {
        self.report
            .add(self.budgets, self.real_gpu, name, value, unit);
    }
}

fn main() -> Result<()> {
    let args = Args::parse();
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
        let app = args.app.clone().unwrap_or_else(|| {
            root.join("target")
                .join("release")
                .join(format!("kraken-pdf{}", std::env::consts::EXE_SUFFIX))
        });
        if !app.is_file() {
            bail!(
                "{} not found. Build it first:\n  cargo build --release -p kraken-pdf --features automation",
                app.display()
            );
        }
        for scenario in &args.scenarios {
            println!("App scenario '{scenario}'...");
            if let Err(e) = app_scenario(&app, scenario, &fixtures, &out, &mut ctx) {
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

fn app_scenario(
    app: &Path,
    scenario: &str,
    fixtures: &Fixtures,
    out: &Path,
    ctx: &mut Ctx,
) -> Result<()> {
    std::fs::create_dir_all(out)?;
    let report_path = out.join(format!("app-{scenario}.json"));
    let _ = std::fs::remove_file(&report_path);

    let spawned_unix_ms = unix_ms();
    let child = Command::new(app)
        .arg(&fixtures.text)
        .env("KRAKEN_AUTOMATION", scenario)
        .env("KRAKEN_PERF_REPORT", &report_path)
        .spawn()
        .with_context(|| format!("starting {}", app.display()))?;
    let samples = sample_until_exit(child)?;

    let text = std::fs::read_to_string(&report_path)
        .context("the app exited without writing its report")?;
    let report: Value = serde_json::from_str(&text)?;
    if report["ok"] != Value::Bool(true) {
        bail!("{}", report["failure"]);
    }
    let rss: Vec<f64> = samples.iter().map(|s| s.rss_mb).collect();
    let mut soak_peaks = Vec::new();

    if scenario == "startup" {
        let first_page = report["first_page_unix_ms"].as_f64().unwrap_or(f64::NAN);
        ctx.add(
            "app.startup.first_page_ms",
            first_page - spawned_unix_ms,
            "ms",
        );
        ctx.add("app.startup.peak_rss_mb", max(&rss), "MB");
    }

    for phase in report["phases"].as_array().into_iter().flatten() {
        let name = phase["name"].as_str().unwrap_or("?");
        let start = phase["start_unix_ms"].as_f64().unwrap_or(0.0);
        let end = phase["end_unix_ms"].as_f64().unwrap_or(f64::MAX);
        // A CPU sample averages the time since the previous sample, so only samples whose
        // whole interval lies inside the phase belong to it.
        let interval = SAMPLE_EVERY.as_secs_f64() * 1000.0;
        let from = if name == "idle" {
            start + IDLE_GRACE_MS
        } else {
            start
        };
        let in_phase: Vec<&Sample> = samples
            .iter()
            .filter(|s| s.unix_ms - interval >= from && s.unix_ms <= end)
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
                ctx.add(&key("fps"), 1000.0 / mean(&intervals).max(1e-9), "fps");
                ctx.add(&key("frame_p99_ms"), percentile(&intervals, 0.99), "ms");
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

/// Samples the child's CPU and memory until it exits.
fn sample_until_exit(mut child: Child) -> Result<Vec<Sample>> {
    let pid = Pid::from_u32(child.id());
    let mut system = System::new();
    let refresh = ProcessRefreshKind::nothing().with_cpu().with_memory();
    let started = Instant::now();
    let mut samples = Vec::new();
    loop {
        if child.try_wait()?.is_some() {
            return Ok(samples);
        }
        if started.elapsed() > APP_TIMEOUT {
            let _ = child.kill();
            bail!("timed out after {} s", APP_TIMEOUT.as_secs());
        }
        system.refresh_processes_specifics(ProcessesToUpdate::Some(&[pid]), true, refresh);
        if let Some(process) = system.process(pid) {
            samples.push(Sample {
                unix_ms: unix_ms(),
                cpu_pct: process.cpu_usage() as f64,
                rss_mb: process.memory() as f64 / (1024.0 * 1024.0),
            });
        }
        std::thread::sleep(SAMPLE_EVERY);
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
