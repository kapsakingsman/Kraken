# Performance testing

Performance is tested at three levels. All test code lives in `crates/perf`; the app only
contains a small scripting hook behind the `automation` cargo feature, which normal builds
do not include.

| Level | Tool | What it answers |
|---|---|---|
| Micro-benchmarks | `cargo bench -p perf` (criterion) | How long does one operation take, and did a change make it slower? |
| Engine suite | `perf-runner --suite engine` | How fast does PDFium render real kinds of pages (text, vector, images)? |
| App suite | `perf-runner --suite app` | How does the real app process behave: startup, frame timing, CPU, memory, leaks? |

`perf-runner` compares every measurement with the budgets in
[`perf/budgets.toml`](../perf/budgets.toml) and fails if one is exceeded.

## Running it

```powershell
# 1. Build the app with the scripting hook (normal builds do not have it).
cargo build --release -p kraken-pdf --features automation

# 2. Run everything: fixtures are generated, then the engine and app suites run.
cargo run --release -p perf

# Only some parts:
cargo run --release -p perf -- --suite engine
cargo run --release -p perf -- --suite app --scenarios scroll,idle

# Micro-benchmarks (criterion keeps history and reports changes between runs):
cargo bench -p perf
```

Reports go to `target/perf-report/`: `report.md`, `report.json`, and the raw report of each
app scenario (`app-<scenario>.json`, with every frame's timing).

Close other heavy programs first and keep the laptop plugged in; power saving modes change
the results. Frame timing budgets are only meaningful on a real GPU with the monitor at its
normal refresh rate. On CI (no GPU) run with `--gpu software`: frame timing is then reported
but not enforced.

## Test documents

Generated on the fly (`crates/perf/src/fixtures.rs`), identical on every run:

| Fixture | Pages | Stresses |
|---|---|---|
| `text-500-pages.pdf` | 500 | Text rendering, and scrolling/memory over a long document |
| `vector-heavy.pdf` | 10 | ~20,000 path segments per page, like CAD drawings or maps |
| `image-heavy.pdf` | 5 | A full-page 1200×1700 uncompressed image per page |

## App scenarios

`perf-runner` starts the app once per scenario, samples the process's CPU and memory from
outside every 250 ms, and reads the frame timing the app writes at the end.

| Scenario | What happens | Main checks |
|---|---|---|
| `startup` | Open the 500-page PDF, 4 times | Time from process start to the first sharp page (first run cold, median of the others), split into steps; memory |
| `scroll` | 8 s scrolling at 2400 screen points/s | Missed frames, UI CPU per frame |
| `zoom` | 100% → 800% → 50% with pauses | Missed frames, memory |
| `sharpen` | Six 0.4 s zoom gestures to different zoom levels | Time from the end of the gesture until every visible tile is sharp |
| `idle` | Wait for prefetching to finish, then 6 s of nothing | CPU use and frames drawn (must be ~0) |
| `tour` | Scroll through all 500 pages | Tile cache stays at its budget; memory |
| `soak` | The zoom sweep 5 times in one process | Memory must not keep growing (leaks) |

Idle CPU is measured from 1 s after the last activity until 0.5 s before the test wakes
the app to end the phase: those two frames are drawn by the test itself, not by idling.

## Results

Linux container with software rendering (lavapipe), `--gpu software`. Frame timing here
says nothing about a real GPU; the Windows PC with its 144 Hz monitor is the reference for
those numbers.

| Metric | Result | Budget |
|---|---:|---:|
| Open 500-page PDF | 3 ms | ≤ 250 ms |
| Text tile, p95 (150% zoom, 150% display) | 1.1 ms | ≤ 15 ms |
| Vector-heavy page, slowest | 129 ms | ≤ 1500 ms |
| Start to first sharp page | 203 ms | ≤ 3000 ms |
| Idle CPU | 0% of a core (Linux and Windows CI) | ≤ 1% |
| Frames drawn while idle | 1 | ≤ 2 |
| Memory, zoom sweep | 174 MB | ≤ 400 MB |
| Memory growth, soak rounds 2–5 | none (−5 MB) | ≤ 32 MB |
| Tile cache after all 500 pages | 300 MB (the cap) | ≤ 300 MB |

Per-frame viewer work (criterion): finding the visible pages of a 10,000-page document takes
35 ns and a tile cache frame at full budget 10 µs, against a frame budget of 6,900 µs at
144 Hz.

### Development PC (Windows 11, real GPU, monitor at 75 Hz)

| Metric | Result |
|---|---:|
| First sharp page, warm / cold | 381 ms / 620 ms |
| Scroll: missed frames | 0.34% |
| Scroll: slowest 1% of frames vs a normal frame | 1.08× |
| Scroll: UI CPU per frame, p99 | 1.35 ms |
| Scroll: whole process CPU | 6% of a core |
| Zoom: missed frames / UI CPU p99 | 0.45% / 1.97 ms |
| Idle CPU | 0% |
| Peak memory (scroll, zoom, 500-page tour) | 200 / 199 / 219 MB |
| Memory growth over 5 zoom sweeps | 1.2 MB |

The frame budget was first a fixed 10.4 ms (1.5 frames at 144 Hz), which failed on this
monitor running at 75 Hz, where a normal frame takes 13.3 ms. Frame timing is now judged
against the monitor's own refresh rate.

### Startup breakdown

The first sharp page takes 0.2 s on the Linux container but 1.7 s on the Windows CI
runner. Both render with the CPU instead of a GPU; the breakdown shows where the time goes:

| Step | Linux | Windows CI | Why Windows CI is slower |
|---|---:|---:|---|
| Process start → `main` | 2 ms | 91 ms | Loading a new, unsigned `.exe` and DLLs (antivirus scan) |
| `main` → window and GPU ready | 50 ms | 418 ms | Setting up the software GPU (WARP) |
| Window → PDF opened | 45 ms | 326 ms | Opening takes 7 ms, but the result is picked up on the next frame, and a frame takes ~300 ms on WARP |
| Opened → first sharp page | 120 ms | 881 ms | 5 frames on both; each frame is ~25 ms on Linux and ~175 ms on WARP |

So most of the difference is frame time under software rendering, not PDF work: opening
the PDF and rendering its tiles take a few milliseconds on both. On a real GPU a frame
takes a few milliseconds, so the same 5–6 frames cost about 30–50 ms. The real number
comes from running `perf-runner --scenarios startup` on a machine with a GPU.

## Problems these tests found

1. **Finished tiles piled up waiting for upload.** During a zoom, up to 434 rendered tiles
   (404 MB) waited in the upload queue, most of them for views the user had already left.
   Now tiles nobody asks for any more are dropped before upload: peak queue 12 tiles.
2. **Tiles were rendered at a scale about to be replaced.** While a zoom gesture was still
   going, the app kept requesting tiles at the old scale for newly visible areas, 16 times
   as many when zooming out from 800%. Now tiles are only requested once the zoom has
   settled; during the gesture the existing tiles are stretched, as designed. For the zoom
   scenario this cut rendered tiles from 1730 to 69 and peak memory from 918 MB to 174 MB.
3. **Slow GPU setup on Windows.** By default wgpu sets up Vulkan, Direct3D 12 and OpenGL
   and then picks one. On the development PC (warm starts, median of 3) that cost:

   | | All backends | Direct3D 12 only |
   |---|---:|---:|
   | Window and GPU ready | 410 ms | 281 ms |
   | First sharp page | 493 ms | 386 ms |
   | Cold start, first sharp page | 718 ms | 462 ms |
   | Peak memory | 209 MB | 172 MB |

   The app now uses only Direct3D 12 on Windows (`WGPU_BACKEND` still overrides it).
4. **Blurry for too long after zooming.** Measured with the `sharpen` scenario (Linux,
   software GPU, median of six gestures):

   | Change | Zoom stop → sharp |
   |---|---:|
   | Before | 265 ms |
   | Upload up to 12 MB of tiles per frame instead of 4 tiles | 185 ms |
   | Wait 80 ms instead of 120 ms for the gesture to end | 150 ms |

5. **The idle test itself was wrong at first.** It started while tiles around the view were
   still being prefetched, which is real work, and on Windows it counted the frame the test
   draws to end the phase (19 samples of 0% and one of 213% under software rendering). The
   idle window now excludes both; the app itself uses 0% CPU when idle on Linux and Windows.
