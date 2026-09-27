//! Micro-benchmarks for per-frame viewer work: `cargo bench -p perf --bench view`.
//! Everything here runs on the UI thread every frame, so it must stay far below 1 ms.

use std::hint::black_box;

use criterion::{Criterion, criterion_group, criterion_main};
use pdf_engine::{DocId, PageSize, Scale, TileKey};
use pdf_view::{DocLayout, TileCache, visible_tiles};

fn benches(c: &mut Criterion) {
    let pages = vec![
        PageSize {
            width_pt: 595.0,
            height_pt: 842.0
        };
        10_000
    ];
    let layout = DocLayout::new(&pages);
    c.bench_function("layout: visible pages in a 10,000-page document", |b| {
        b.iter(|| black_box(layout.visible(black_box(4_000_000.0), black_box(4_001_000.0))))
    });
    c.bench_function("layout: build for 10,000 pages", |b| {
        b.iter(|| black_box(DocLayout::new(black_box(&pages))))
    });
    c.bench_function("visible tiles of one page", |b| {
        b.iter(|| {
            black_box(visible_tiles(
                (12_000, 17_000),
                (-3000.0, -5000.0),
                (1920.0, 1080.0),
                pdf_engine::TILE_SIZE,
            ))
        })
    });

    // A full cache (300 tiles of 1 MB) where each frame draws 40 tiles and adds 4.
    c.bench_function("tile cache: one frame at the memory budget", |b| {
        let mut cache = TileCache::new(300);
        let key = |i: u32| TileKey {
            doc: DocId(1),
            page: i / 16,
            scale: Scale::from_px_per_pt(2.0),
            tx: i % 4,
            ty: (i / 4) % 4,
            size: pdf_engine::TILE_SIZE,
        };
        let mut next = 0u32;
        b.iter(|| {
            cache.begin_frame();
            for i in next.saturating_sub(40)..next {
                black_box(cache.get(&key(i)));
            }
            for _ in 0..4 {
                cache.insert(key(next), (), 1);
                next += 1;
            }
        })
    });
}

criterion_group!(view, benches);
criterion_main!(view);
