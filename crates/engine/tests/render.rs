//! End-to-end tests against the real PDFium library (run `scripts/fetch-pdfium` first).

use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Duration;

use pdf_engine::geometry::{page_px_size, tile_grid, tile_rect};
use pdf_engine::{
    DocInfo, Engine, EngineConfig, EngineError, Scale, TileKey, TileRequest, locate_pdfium,
};
use pdfium_render::prelude::*;

// PDFium is process-global, so these tests take turns instead of running in parallel.
static SERIAL: Mutex<()> = Mutex::new(());

const TIMEOUT: Duration = Duration::from_secs(30);

fn pdfium() -> Pdfium {
    let dir = locate_pdfium().expect("run scripts/fetch-pdfium before the tests");
    match Pdfium::bind_to_library(Pdfium::pdfium_platform_library_name_at_path(&dir)) {
        Ok(bindings) => Pdfium::new(bindings),
        Err(PdfiumError::PdfiumLibraryBindingsAlreadyInitialized) => Pdfium::default(),
        Err(e) => panic!("bind PDFium: {e:?}"),
    }
}

/// Writes a 3-page PDF: A4, A4 with /Rotate 90, and a small 300x200 pt page. Each page has
/// a red square at (50..100, 100..150) pt, text, and diagonal lines that cross tile borders.
fn write_fixture(path: &Path) {
    let pdfium = pdfium();
    let mut document = pdfium.create_new_pdf().unwrap();
    let font = document.fonts_mut().helvetica();
    let sizes = [
        PdfPagePaperSize::a4(),
        PdfPagePaperSize::a4(),
        PdfPagePaperSize::from_points(PdfPoints::new(300.0), PdfPoints::new(200.0)),
    ];
    for (index, size) in sizes.into_iter().enumerate() {
        let mut page = document.pages_mut().create_page_at_end(size).unwrap();
        let (w, h) = (page.width(), page.height());
        let objects = page.objects_mut();
        objects
            .create_path_object_rect(
                PdfRect::new_from_values(100.0, 50.0, 150.0, 100.0),
                None,
                None,
                Some(PdfColor::RED),
            )
            .unwrap();
        objects
            .create_path_object_line(
                PdfPoints::ZERO,
                PdfPoints::ZERO,
                w,
                h,
                PdfColor::BLACK,
                PdfPoints::new(1.5),
            )
            .unwrap();
        objects
            .create_path_object_line(
                PdfPoints::ZERO,
                h,
                w,
                PdfPoints::ZERO,
                PdfColor::BLUE,
                PdfPoints::new(3.0),
            )
            .unwrap();
        objects
            .create_text_object(
                PdfPoints::new(20.0),
                h - PdfPoints::new(40.0),
                "Kraken PDF tile test",
                font,
                PdfPoints::new(18.0),
            )
            .unwrap();
        if index == 1 {
            page.set_rotation(PdfPageRenderRotation::Degrees90);
        }
    }
    document.save_to_file(path).unwrap();
}

struct Fixture {
    _dir: tempfile::TempDir,
    path: PathBuf,
}

fn fixture(file_name: &str) -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join(file_name);
    write_fixture(&path);
    Fixture { _dir: dir, path }
}

fn engine() -> Engine {
    Engine::start(EngineConfig::default()).expect("start engine")
}

/// Renders every tile of a page through the engine and stitches them into one RGBA image.
fn render_stitched(engine: &Engine, doc: &DocInfo, page: u32, scale: Scale) -> (u32, u32, Vec<u8>) {
    let page_px = page_px_size(doc.page_sizes[page as usize], scale);
    let (cols, rows) = tile_grid(page_px);
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
    let mut image = vec![0u8; page_px.0 as usize * page_px.1 as usize * 4];
    for _ in 0..cols * rows {
        let result = engine.results().recv_timeout(TIMEOUT).expect("tile result");
        let tile = result.tile.expect("tile rendered");
        let rect = tile_rect(page_px, result.key.tx, result.key.ty).unwrap();
        assert_eq!((tile.width, tile.height), (rect.width, rect.height));
        assert_eq!(tile.rgba.len(), (rect.width * rect.height * 4) as usize);
        for row in 0..rect.height as usize {
            let src = &tile.rgba[row * rect.width as usize * 4..][..rect.width as usize * 4];
            let dst_start = ((rect.y as usize + row) * page_px.0 as usize + rect.x as usize) * 4;
            image[dst_start..dst_start + src.len()].copy_from_slice(src);
        }
    }
    (page_px.0, page_px.1, image)
}

/// Renders the whole page in one call, the straightforward way, as the reference image.
fn render_reference(path: &Path, page: u32, page_px: (u32, u32)) -> Vec<u8> {
    let pdfium = pdfium();
    let document = pdfium.load_pdf_from_file(path, None).unwrap();
    let page = document.pages().get(page as PdfPageIndex).unwrap();
    let config = PdfRenderConfig::new()
        .set_fixed_size(page_px.0 as Pixels, page_px.1 as Pixels)
        .set_clear_color(PdfColor::WHITE)
        .render_form_data(true)
        .render_annotations(true)
        .set_reverse_byte_order(true);
    page.render_with_config(&config).unwrap().as_rgba_bytes()
}

fn mean_abs_diff(a: &[u8], b: &[u8]) -> f64 {
    assert_eq!(a.len(), b.len());
    let total: u64 = a.iter().zip(b).map(|(x, y)| x.abs_diff(*y) as u64).sum();
    total as f64 / a.len() as f64
}

#[test]
fn page_sizes_include_rotation() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let fixture = fixture("sizes.pdf");
    let doc = engine().open(&fixture.path, None).unwrap();

    let sizes: Vec<(i32, i32)> = doc
        .page_sizes
        .iter()
        .map(|s| (s.width_pt.round() as i32, s.height_pt.round() as i32))
        .collect();
    assert_eq!(sizes, vec![(595, 842), (842, 595), (300, 200)]);
}

#[test]
fn stitched_tiles_match_a_full_page_render() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let fixture = fixture("stitch.pdf");
    let engine = engine();
    let doc = engine.open(&fixture.path, None).unwrap();

    // 2.0 px/pt gives an A4 page a 3x4 grid with partial tiles on the right and bottom.
    for scale in [Scale::from_px_per_pt(2.0), Scale::from_zoom(137.0, 1.25)] {
        for page in 0..3 {
            let (w, h, stitched) = render_stitched(&engine, &doc, page, scale);
            let reference = render_reference(&fixture.path, page, (w, h));
            let diff = mean_abs_diff(&stitched, &reference);
            assert!(
                diff < 0.02,
                "page {page} at {scale:?}: tiles differ from the full render by {diff:.3}/255"
            );
            let ink = stitched.chunks_exact(4).filter(|p| p[0] < 200).count();
            assert!(ink > 100, "page {page} rendered blank");
        }
    }
}

#[test]
fn tiles_are_rgba_and_opaque() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let fixture = fixture("colors.pdf");
    let engine = engine();
    let doc = engine.open(&fixture.path, None).unwrap();
    let scale = Scale::from_px_per_pt(2.0);
    let (w, _, image) = render_stitched(&engine, &doc, 0, scale);

    let pixel = |x_pt: f32, y_pt: f32| {
        let height_pt = doc.page_sizes[0].height_pt;
        let x = (x_pt * 2.0) as usize;
        let y = ((height_pt - y_pt) * 2.0) as usize; // PDF y grows upward
        let i = (y * w as usize + x) * 4;
        [image[i], image[i + 1], image[i + 2], image[i + 3]]
    };
    assert_eq!(pixel(75.0, 125.0), [255, 0, 0, 255], "red square");
    assert_eq!(
        pixel(500.0, 400.0),
        [255, 255, 255, 255],
        "white background"
    );
}

#[test]
fn opens_files_with_non_ascii_names() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let fixture = fixture("हिंदी फ़ाइल.pdf");
    let doc = engine().open(&fixture.path, None).unwrap();
    assert_eq!(doc.page_sizes.len(), 3);
}

#[test]
fn stale_generations_are_not_rendered() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let fixture = fixture("generation.pdf");
    let engine = engine();
    let doc = engine.open(&fixture.path, None).unwrap();
    let key = |tx| TileKey {
        doc: doc.id,
        page: 0,
        scale: Scale::from_px_per_pt(1.0),
        tx,
        ty: 0,
    };

    engine.set_generation(5);
    engine.request_tile(TileRequest {
        key: key(0),
        generation: 4,
        priority: 0,
    });
    engine.request_tile(TileRequest {
        key: key(1),
        generation: 5,
        priority: 1,
    });

    let result = engine.results().recv_timeout(TIMEOUT).unwrap();
    assert_eq!((result.key.tx, result.generation), (1, 5));
    assert!(
        engine
            .results()
            .recv_timeout(Duration::from_millis(300))
            .is_err(),
        "stale tile was rendered"
    );
}

#[test]
fn reports_errors_instead_of_panicking() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let engine = engine();
    let dir = tempfile::tempdir().unwrap();

    let missing = engine.open(dir.path().join("missing.pdf"), None);
    assert!(matches!(missing, Err(EngineError::Io(_))), "{missing:?}");

    let junk = dir.path().join("junk.pdf");
    std::fs::write(&junk, b"this is not a pdf").unwrap();
    assert!(matches!(
        engine.open(&junk, None),
        Err(EngineError::Open(_))
    ));

    let fixture = fixture("errors.pdf");
    let doc = engine.open(&fixture.path, None).unwrap();
    let scale = Scale::from_px_per_pt(1.0);
    for (page, tx) in [(7, 0), (0, 99)] {
        engine.request_tile(TileRequest {
            key: TileKey {
                doc: doc.id,
                page,
                scale,
                tx,
                ty: 0,
            },
            generation: 0,
            priority: 0,
        });
        let result = engine.results().recv_timeout(TIMEOUT).unwrap();
        assert!(result.tile.is_err());
    }

    engine.close(doc.id);
    engine.request_tile(TileRequest {
        key: TileKey {
            doc: doc.id,
            page: 0,
            scale,
            tx: 0,
            ty: 0,
        },
        generation: 0,
        priority: 0,
    });
    let result = engine.results().recv_timeout(TIMEOUT).unwrap();
    assert!(matches!(result.tile, Err(EngineError::UnknownDocument)));
}
