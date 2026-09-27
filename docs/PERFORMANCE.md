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
# Run everything: perf-runner builds the app with its scripting hook (into
# target/perf-app, separate from normal builds), generates the fixtures, then runs the
# engine and app suites.
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
| `wheel` | Seven Ctrl+wheel notches, sent as real input events | Time from the notch until every visible tile is sharp |
| `spin` | Four fast Ctrl+wheel spins of 8 notches (one every 60 ms) | Time from the last notch to sharp; tiles rendered and memory for the steps passed through |
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
| Zoom stop → sharp, median / slowest | 107 ms / 161 ms (80 ms of it is the gesture-end wait) |
| Zoom: UI CPU p99 with the 12 MB upload budget | 1.31 ms |

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

5. **A PowerPoint export with a 151-megapixel image mask** (`issue16263.pdf`, a known
   PowerPoint 2013 bug: a 2×2 image with a 34,862 × 4,332 soft mask). PDFium decodes and
   downscales the mask on every render call; it is too big for PDFium's image cache. The
   time is almost all PDFium's high-quality image downscaling ("image smoothing"), which
   only changes how images look. Now:
   - previews are always rendered without image smoothing,
   - on pages that draw images, sharp tiles come first as a draft without image smoothing
     and are replaced by the final render as soon as all missing tiles are done,
   - pages without images render once, as before.

   | This PDF, Linux software GPU | Before | After |
   |---|---:|---:|
   | Start to first sharp page | 3.27 s | 1.07 s |
   | Zoom stop to sharp, median | 964 ms | 507 ms |
   | Zoom stop to sharp, slowest | 2031 ms | 674 ms |

   Each render of this page still costs at least ~0.4 s for decoding the mask alone.

   Test any PDF with `cargo run --release -p perf -- --suite app --pdf file.pdf` and the
   engine alone with `pdf-cli bench file.pdf [--draft]`.
6. **Ctrl+wheel zoom went blurry before turning sharp; Acrobat does not.** egui smooths
   Ctrl+wheel into a zoom that keeps changing for about 0.15 s after the notch, and tiles
   were only rendered 80 ms after it stopped. So every notch showed stretched, blurry tiles
   for about a third of a second. Acrobat treats a notch as a finished action: it jumps to
   the next zoom step and renders it at once. Now Kraken does the same: each notch jumps to
   the next zoom step (100 → 125 → 150 → 200 …) around the mouse pointer and renders right
   away. Touchpad pinches still zoom smoothly and render when the fingers stop.

   | `wheel` scenario, Linux software GPU | Before | After |
   |---|---:|---:|
   | Notch to sharp, median | 347 ms | 36 ms |
   | Notch to sharp, slowest | 413 ms | 89 ms |

   The cost shows up on fast spins (`spin` scenario): every zoom step passed through is
   rendered, where before only the final one was.

   | Four fast spins, Linux software GPU | Before | After |
   |---|---:|---:|
   | Last notch to sharp, median | 331 ms | 18 ms |
   | Tiles rendered | 33 | ~160 |
   | Tile cache, peak | 25 MB | 124 MB |
   | Process memory, peak | 186 MB | ~300 MB |

   The extra tiles stay within the 300 MB tile cache and are evicted as usual; for single
   notches (the `wheel` scenario) tiles rendered and memory are the same as before.
7. **A slow tile kept rendering after the view had moved on.** PDFium renders a tile in one
   call, so once a slow tile had started (up to ~0.8 s on the huge-mask PDF), a new zoom
   step or scroll position had to wait for it. The engine now renders through PDFium's
   progressive API: PDFium asks the engine every few milliseconds whether to continue, even
   while scaling a large image, and the engine stops when the UI's new set of wanted tiles
   no longer contains the tile. (pdfium-render does not expose this API, so a copy with the
   addition lives in `third_party/`.)

   | Stopping a tile nobody wants any more | Full render | Stopped after |
   |---|---:|---:|
   | 60,000 small paths (engine test) | 530 ms | 0.2 ms |
   | Huge-mask PDF, final quality | 765 ms | 9–35 ms |

   | Huge-mask PDF, Linux software GPU | Before | After |
   |---|---:|---:|
   | Fast spins: tiles rendered to the end | 74 | 59 (31 stopped part way) |
   | Fast spins: last notch to sharp, slowest | 168 ms | 134 ms |
   | Wheel notches: notch to sharp, slowest | 888 ms | 696 ms |

   Text and vector tiles take 1–10 ms, so they finish before anything can cancel them: for
   ordinary PDFs this changes nothing, including the memory taken on fast spins.
8. **The idle test itself was wrong at first.** It started while tiles around the view were
   still being prefetched, which is real work, and on Windows it counted the frame the test
   draws to end the phase (19 samples of 0% and one of 213% under software rendering). The
   idle window now excludes both; the app itself uses 0% CPU when idle on Linux and Windows.
9. **Some tiles were rendered twice.** The view asked again for tiles whose result it had
   not read yet (8–10 tiles per zoom scenario, ~9% in `sharpen`). The view now tells the
   engine how many results it has read, and the engine skips requests for tiles it has
   already sent. `tiles_duplicated` is now 0 in every scenario, with a budget of 0.
10. **PDFium only started once the window was ready.** Creating the window and the GPU
    device takes ~270 ms on the development PC; opening the file and rendering the first
    page came after that. They now run side by side: a startup thread starts PDFium,
    opens the file and renders the first page before the window exists. The tiles need
    the display scale, which the app remembers from the last run (on the very first start
    only the preview is rendered ahead).

    | Start to first sharp page, Linux software GPU | Before | After |
    |---|---:|---:|
    | Warm (median of 3) | 167 ms | ~120 ms |
    | Cold | 179 ms | ~115 ms |

    The startup run became too short for 250 ms sampling to catch its memory peak, so
    perf-runner now reads memory every 10 ms.
11. **One CPU core rendered everything.** PDFium can only be used from one thread per
    process, so slow pages now also render in helper processes (see
    [ARCHITECTURE.md](ARCHITECTURE.md#render-workers)). Ordinary pages never use them.

    | `issue16263.pdf`, Linux software GPU (4 cores, 2 helpers) | 1 process | With helpers |
    |---|---:|---:|
    | Zoom stop to sharp, median | 502 ms | 380 ms |
    | Zoom stop to sharp, slowest | 757 ms | 487 ms |
    | Ctrl+wheel notch to sharp, slowest | 696 ms | 397 ms |
    | Helpers' memory while rendering / on standby | – | 133 MB / 18 MB |

    On this machine the software GPU (llvmpipe) competes for the same 4 cores; with a real
    GPU the helpers have the cores to themselves. With the 500-page text fixture the
    helpers render no tiles at all, and idle CPU stays at 0%.
12. **An A1 construction plan took 2.1 s to show its first page.** A few tiles of such a
    drawing take 250–400 ms each (the dense details), the rest almost nothing. Three
    changes (Linux, software GPU, the generated `plan-a1.pdf` fixture, tiles up to 250 ms):
    - A page counts as slow as soon as a tile has been rendering for 25 ms, instead of
      after it finished, and the in-process engine hands back the other tiles of that page
      it had already taken, so the helpers share them.
    - Slow pages use 256-pixel tiles: a dense area splits into four tiles that render on
      different processes (about 14% more work in total, which is why ordinary pages keep
      512-pixel tiles).
    - Idle helpers free their caches after 3 s instead of 10 s: each holds its own copy of
      the parsed page, which on such drawings is tens of megabytes.

    | `plan-a1.pdf`, Linux software GPU (4 cores) | Before | After |
    |---|---:|---:|
    | Start to first sharp page | 992 ms | 490 ms |
    | Zoom stop to sharp, median | 335 ms | 347 ms |

    On this 4-core machine, zooming gains nothing: the software GPU needs the same cores.
    The screen stays pixel-for-pixel identical to PDFium's own render with the small tiles.
13. **`--pdf` runs failed on budgets made for the fixtures.** With `--pdf` the budgets are
    now shown for comparison ("over (not enforced for --pdf)") and do not fail the run.
14. **Audit fixes** (after a review of the whole pipeline):
    - *Previews of slow pages were the most expensive render.* A preview draws every
      object of the page; on the plan fixture it takes 510 ms, a 512-pixel tile of it
      0.4 ms. On slow pages the preview now waits for the sharp tiles on screen, and one
      already rendering is stopped.
    - *Freeing helpers' caches did not free memory.* PDFium's freed memory stays with the
      process. Idle helpers now stop after 5 s instead: on the plan fixture the app with
      its helpers went from 257 MB to 179 MB five seconds after the last zoom.
    - *Every helper holds its own copy of the parsed page* (100 MB or more on large
      drawings). The pool reads each helper's memory from the system and keeps the helpers
      together at about 256 MB.
    - *Pages were marked slow too easily.* One tile of 25 ms, including parsing the page,
      was enough; a busy machine could turn ordinary pages slow for good. Tile times now
      exclude parsing (`Tile::load_time`), it takes one tile of 75 ms or two of 25 ms, and
      16 quick tiles in a row make a page ordinary again.
    - The GPU memory allocator is asked to favour low memory use (`MemoryHints::MemoryUsage`);
      no effect with software rendering, to be measured on a real GPU.
    - The HUD shows the memory of the app and of its helpers, and the idle test reports the
      CPU time of each thread, to find what wakes an idle app.
15. **The tests could report numbers that did not mean what they said** (found by an
    outside review):
    - *"Sharp" included drafts.* A draft tile (rendered without image smoothing, replaced
      by the final one moments later) counted as done, and once a view was complete it
      stayed complete even after scrolling to tiles that were not there yet. Completion is
      now worked out every frame from what is on screen, and only final tiles count. The
      startup report shows both moments: `first_coverage_ms` (every visible tile there,
      drafts included) and `first_page_ms` (every visible tile final).
    - *Wheel and spin timings read the previous frame.* A notch changes the zoom at once,
      and the test looked at the completion flag before the frame for the new zoom was
      drawn, so it could see the old view's "complete". The test now looks after the frame
      is drawn. Figures measured with the old test (wheel ~13 ms per notch on the
      development PC, 36 ms in the table under 6) were too low; with the fix the same
      Linux run gives 46 ms median, 68 ms slowest.
    - *A phase that never finished passed.* Sharpen, wheel, spin and settle phases now fail
      when the view is not complete in time, and a missing measurement is an error
      instead of a zero.
    - *Missed frames were judged against the app's own frame rate.* An app steadily drawing
      72 fps on a 144 Hz monitor looked perfect. On Windows the app now reads the monitor's
      refresh rate and judges frames against it (HUD and `display_hz`); the measured rate
      is reported separately as `cadence_hz`.
    - *`--pdf` runs said PASS* although budgets were not enforced; they now say
      "PASS (advisory …)". The first startup run is called "first run" rather than
      "cold": disk and shader caches are not controlled.
    - *A failed render could count as a finished tile.* The progressive render now reports
      PDFium's `FPDF_RENDER_FAILED` as an error instead of returning the incomplete image.
    - The HUD is hidden until F3 is pressed.
