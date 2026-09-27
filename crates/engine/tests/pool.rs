//! The render pool with real worker processes (the `pdf-render-worker` binary of this
//! crate). Needs PDFium like the other tests (run `scripts/fetch-pdfium` first).

use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use pdf_engine::geometry::{page_px_size, tile_grid};
use pdf_engine::{
    DocInfo, EngineError, PoolConfig, Quality, RenderPool, Scale, TileKey, TileRequest, TileResult,
    WorkerCommand,
};

// PDFium is process-global in this test process too.
static SERIAL: Mutex<()> = Mutex::new(());

const TIMEOUT: Duration = Duration::from_secs(60);

fn worker() -> WorkerCommand {
    WorkerCommand {
        program: PathBuf::from(env!("CARGO_BIN_EXE_pdf-render-worker")),
        args: Vec::new(),
    }
}

fn pool(worker: Option<WorkerCommand>, max_helpers: usize) -> RenderPool {
    RenderPool::start(
        PoolConfig {
            worker,
            max_helpers,
            ..PoolConfig::default()
        },
        || {},
    )
    .expect("start pool")
}

/// Writes a PDF from numbered objects (object 1 is the catalog).
fn write_pdf(path: &Path, objects: &[Vec<u8>]) {
    let mut out = b"%PDF-1.7\n".to_vec();
    let mut offsets = Vec::new();
    for (i, body) in objects.iter().enumerate() {
        offsets.push(out.len());
        out.extend_from_slice(format!("{} 0 obj\n", i + 1).as_bytes());
        out.extend_from_slice(body);
        out.extend_from_slice(b"\nendobj\n");
    }
    let xref = out.len();
    out.extend_from_slice(
        format!("xref\n0 {}\n0000000000 65535 f \n", objects.len() + 1).as_bytes(),
    );
    for offset in offsets {
        out.extend_from_slice(format!("{offset:010} 00000 n \n").as_bytes());
    }
    out.extend_from_slice(
        format!(
            "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n",
            objects.len() + 1
        )
        .as_bytes(),
    );
    std::fs::write(path, out).unwrap();
}

fn stream(body: &[u8]) -> Vec<u8> {
    [
        format!("<< /Length {} >>\nstream\n", body.len()).into_bytes(),
        body.to_vec(),
        b"\nendstream".to_vec(),
    ]
    .concat()
}

/// Page 1: small stroked paths all over the page, so every tile is slow. Page 2: one line
/// of text, so its tiles are fast.
fn write_fixture(path: &Path) {
    let mut content = String::new();
    let mut seed: u32 = 11;
    let mut next = |range: f32| {
        seed = seed.wrapping_mul(1_103_515_245).wrapping_add(12_345);
        (seed >> 16) as f32 / 65_536.0 * range
    };
    for _ in 0..80_000 {
        let (x, y) = (next(595.0), next(842.0));
        content.push_str(&format!(
            "{x:.1} {y:.1} m {:.1} {:.1} l {:.1} {:.1} l S\n",
            x + next(20.0),
            y + next(20.0),
            x + next(20.0),
            y - next(20.0)
        ));
    }
    write_pdf(
        path,
        &[
            b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(),
            b"<< /Type /Pages /Kids [3 0 R 4 0 R] /Count 2 >>".to_vec(),
            b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 595 842] /Contents 5 0 R >>".to_vec(),
            b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 595 842] /Resources << /Font << /F1 7 0 R >> >> /Contents 6 0 R >>".to_vec(),
            stream(content.as_bytes()),
            stream(b"BT /F1 24 Tf 72 700 Td (Fast page) Tj ET"),
            b"<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>".to_vec(),
        ],
    );
}

struct Fixture {
    _dir: tempfile::TempDir,
    path: PathBuf,
}

fn fixture() -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("pool.pdf");
    write_fixture(&path);
    Fixture { _dir: dir, path }
}

fn all_tiles(doc: &DocInfo, page: u32, scale: Scale) -> Vec<TileRequest> {
    let (cols, rows) = tile_grid(
        page_px_size(doc.page_sizes[page as usize], scale),
        pdf_engine::TILE_SIZE,
    );
    (0..rows)
        .flat_map(|ty| (0..cols).map(move |tx| (tx, ty)))
        .map(|(tx, ty)| TileRequest {
            key: TileKey {
                doc: doc.id,
                page,
                scale,
                tx,
                ty,
                size: pdf_engine::TILE_SIZE,
            },
            generation: 0,
            priority: tx + ty,
            quality: Quality::Final,
        })
        .collect()
}

fn collect(pool: &RenderPool, count: usize) -> Vec<TileResult> {
    let mut results: Vec<TileResult> = (0..count)
        .map(|_| pool.results().recv_timeout(TIMEOUT).expect("tile result"))
        .collect();
    results.sort_by_key(|r| r.key);
    results
}

fn wait_until(what: &str, mut done: impl FnMut() -> bool) {
    let deadline = Instant::now() + TIMEOUT;
    while !done() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn ordinary_pages_stay_in_process() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let fixture = fixture();
    let pool = pool(Some(worker()), 2);
    let doc = pool.open(&fixture.path, None).unwrap();
    pool.prewarm();
    wait_until("helpers to start", || pool.status().helpers == 2);

    let requests = all_tiles(&doc, 1, Scale::from_px_per_pt(1.5));
    let count = requests.len();
    pool.set_wanted(1, requests, Some(0));
    for result in collect(&pool, count) {
        result.tile.unwrap();
    }
    assert_eq!(
        pool.status().tiles_by_helpers,
        0,
        "fast tiles are not worth sending to another process"
    );
}

#[test]
fn slow_pages_are_spread_over_helpers_and_render_identically() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let fixture = fixture();

    // Reference: everything in-process.
    let alone = pool(None, 0);
    let doc = alone.open(&fixture.path, None).unwrap();
    let scale = Scale::from_px_per_pt(1.5);
    let requests = all_tiles(&doc, 0, scale);
    let count = requests.len();
    alone.set_wanted(1, requests.clone(), Some(0));
    let reference = collect(&alone, count);
    drop(alone);

    let pool = pool(Some(worker()), 2);
    let doc = pool.open(&fixture.path, None).unwrap();
    pool.prewarm();
    wait_until("helpers to start", || pool.status().helpers == 2);
    // The first tile shows the page is slow.
    let mut first = requests[0];
    first.key.doc = doc.id;
    first.key.scale = Scale::from_px_per_pt(1.0);
    pool.set_wanted(1, vec![first], Some(0));
    collect(&pool, 1)[0].tile.as_ref().unwrap();

    let requests: Vec<TileRequest> = requests
        .into_iter()
        .map(|mut r| {
            r.key.doc = doc.id;
            r
        })
        .collect();
    pool.set_wanted(2, requests, Some(1));
    let results = collect(&pool, count);
    assert!(
        pool.status().tiles_by_helpers > 0,
        "slow tiles should use the helpers"
    );
    for (got, want) in results.iter().zip(&reference) {
        let (got_tile, want_tile) = (got.tile.as_ref().unwrap(), want.tile.as_ref().unwrap());
        assert_eq!((got.key.tx, got.key.ty), (want.key.tx, want.key.ty));
        assert!(
            got_tile.rgba == want_tile.rgba,
            "tile {:?} differs between processes",
            (got.key.tx, got.key.ty)
        );
    }
}

#[test]
fn a_crashed_helper_costs_only_a_retry() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let fixture = fixture();
    let pool = pool(Some(worker()), 1);
    let doc = pool.open(&fixture.path, None).unwrap();
    pool.prewarm();
    wait_until("the helper to start", || pool.status().helpers == 1);
    let requests = all_tiles(&doc, 0, Scale::from_px_per_pt(1.0));
    pool.set_wanted(1, vec![requests[0]], Some(0));
    collect(&pool, 1)[0].tile.as_ref().unwrap();

    let requests = all_tiles(&doc, 0, Scale::from_px_per_pt(1.5));
    let count = requests.len();
    pool.set_wanted(2, requests, Some(1));
    wait_until("the helper to get work", || pool.status().busy_helpers == 1);
    pool.kill_helpers();

    for result in collect(&pool, count) {
        result.tile.unwrap();
    }
    assert!(pool.status().helper_crashes >= 1);
}

#[test]
fn a_missing_worker_program_falls_back_to_in_process() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let fixture = fixture();
    let missing = WorkerCommand {
        program: PathBuf::from("this-program-does-not-exist"),
        args: Vec::new(),
    };
    let pool = pool(Some(missing), 2);
    let doc = pool.open(&fixture.path, None).unwrap();
    pool.prewarm();
    let requests = all_tiles(&doc, 1, Scale::from_px_per_pt(1.0));
    let count = requests.len();
    pool.set_wanted(1, requests, Some(0));
    for result in collect(&pool, count) {
        result.tile.unwrap();
    }
    let status = pool.status();
    assert_eq!(status.helpers, 0);
    assert!(status.helper_error.is_some());
}

#[test]
fn idle_helpers_stop() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let fixture = fixture();
    let pool = RenderPool::start(
        PoolConfig {
            worker: Some(worker()),
            max_helpers: 2,
            trim_after: Duration::from_millis(200),
            stop_after: Duration::from_millis(600),
            ..PoolConfig::default()
        },
        || {},
    )
    .unwrap();
    pool.open(&fixture.path, None).unwrap();
    pool.prewarm();
    wait_until("helpers to start", || pool.status().helpers == 2);
    wait_until("idle helpers to stop", || pool.status().helpers == 0);
}

#[test]
fn tiles_nobody_wants_any_more_are_not_delivered() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let fixture = fixture();
    let pool = pool(Some(worker()), 2);
    let doc = pool.open(&fixture.path, None).unwrap();
    pool.prewarm();
    wait_until("helpers to start", || pool.status().helpers == 2);
    let slow = all_tiles(&doc, 0, Scale::from_px_per_pt(1.0));
    pool.set_wanted(1, vec![slow[0]], Some(0));
    collect(&pool, 1)[0].tile.as_ref().unwrap();

    // Ask for a whole slow page, then change our mind right away.
    pool.set_wanted(2, all_tiles(&doc, 0, Scale::from_px_per_pt(2.0)), Some(1));
    std::thread::sleep(Duration::from_millis(30));
    let fast = all_tiles(&doc, 1, Scale::from_px_per_pt(1.0));
    let count = fast.len();
    pool.set_wanted(3, fast, Some(1));

    let deadline = Instant::now() + TIMEOUT;
    let mut fast_done = 0;
    while fast_done < count {
        let result = pool
            .results()
            .recv_timeout(deadline - Instant::now())
            .expect("result");
        match (&result.tile, result.key.page) {
            (Ok(_), 1) => fast_done += 1,
            // A slow tile finished before the change arrived is fine; a cancelled one must
            // not be reported (nobody asked for it any more).
            (Ok(_), _) => {}
            (Err(EngineError::Cancelled), _) => panic!("cancelled tile was delivered"),
            (Err(e), _) => panic!("{e}"),
        }
    }
}

#[test]
fn a_slow_page_is_noticed_while_its_first_tile_renders() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let fixture = fixture();
    let pool = pool(Some(worker()), 2);
    let doc = pool.open(&fixture.path, None).unwrap();
    pool.prewarm();
    wait_until("helpers to start", || pool.status().helpers == 2);

    // Nothing is known about page 1 yet: its first tile alone shows it is slow, long
    // before that tile is done, so the other tiles already go to the helpers.
    let requests = all_tiles(&doc, 0, Scale::from_px_per_pt(1.5));
    let count = requests.len();
    pool.set_wanted(1, requests, Some(0));
    for result in collect(&pool, count) {
        result.tile.unwrap();
    }
    assert!(pool.is_slow(doc.id, 0));
    assert!(
        pool.status().tiles_by_helpers > 0,
        "helpers should have joined in on the first slow page"
    );

    let fast = all_tiles(&doc, 1, Scale::from_px_per_pt(1.0));
    let count = fast.len();
    pool.set_wanted(2, fast, Some(count as u64));
    for result in collect(&pool, count) {
        result.tile.unwrap();
    }
    assert!(!pool.is_slow(doc.id, 1));
}
