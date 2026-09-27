//! Micro-benchmarks for the rendering engine: `cargo bench -p perf --bench engine`.

use std::time::Duration;

use criterion::{Criterion, criterion_group, criterion_main};
use pdf_engine::{DocInfo, Engine, EngineConfig, Scale, TileKey, TileRequest};
use perf::fixtures;

fn render_tile(engine: &Engine, doc: &DocInfo, page: u32, scale: Scale, tx: u32) {
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
    let result = engine
        .results()
        .recv_timeout(Duration::from_secs(30))
        .unwrap();
    result.tile.unwrap();
}

fn benches(c: &mut Criterion) {
    let dir = std::env::temp_dir().join("kraken-perf-fixtures");
    let fixtures = fixtures::ensure(&dir).unwrap();
    let engine = Engine::start(EngineConfig::default()).unwrap();

    c.bench_function("open 500-page PDF", |b| {
        b.iter(|| {
            let doc = engine.open(&fixtures.text, None).unwrap();
            engine.close(doc.id);
        })
    });

    let scale = Scale::from_zoom(150.0, 1.5);
    for (name, path) in [
        ("text", &fixtures.text),
        ("vector", &fixtures.vector),
        ("image", &fixtures.images),
    ] {
        let doc = engine.open(path, None).unwrap();
        // The page stays parsed in the engine's page cache, as when scrolling.
        c.bench_function(&format!("render one 512px tile ({name} page, 150%)"), |b| {
            b.iter(|| render_tile(&engine, &doc, 0, scale, 0))
        });
    }
}

criterion_group! {
    name = engine;
    config = Criterion::default().sample_size(20);
    targets = benches
}
criterion_main!(engine);
