# Architecture

## Processes and threads

```
kraken-pdf (the window)
├─ UI thread ............ input, zoom/scroll, egui, drawing tiles on the GPU (wgpu, D3D12)
├─ startup thread ....... at launch only: starts PDFium, opens the file, first page
├─ open thread .......... while a file opens, so a large file cannot freeze the window
├─ render-pool thread ... decides which renderer renders which tile
├─ pdfium thread ........ the in-process engine: the only thread here that calls PDFium
└─ reader threads ....... one per helper, receiving its tiles
kraken-pdf --render-worker  × 0–3 (helpers, only while useful)
└─ pdfium thread ........ the helper's own PDFium
```

The UI never calls PDFium and never waits for it: it asks for tiles and draws whatever has
arrived, stretching older tiles until the sharp ones come.

## Rendering

Pages are rendered as 512×512 tiles at the exact screen resolution (zoom × display
scale), so a settled view is pixel-for-pixel what PDFium produces. The view asks for the
visible tiles plus one screen above and below; the whole set is sent at once
(`RenderPool::set_wanted`), so the renderers can drop, and stop part way, whatever is no
longer wanted.

Pages with images first get a quick draft (no image smoothing) and then the final tile.

## Render workers

PDFium can only be used from one thread per process. To use more CPU cores, the
`RenderPool` (crates/engine/src/pool.rs) adds helper processes: the app itself started
with `--render-worker`, each with its own PDFium, talking over stdin/stdout
(crates/engine/src/protocol.rs).

| Rule | Value |
|---|---|
| Which tiles go to helpers | Only tiles of *slow* pages. Sending a tile over costs 1–2 ms, so ordinary pages (1–10 ms per tile) never leave the process |
| Slow page | Drawing times only (parsing the page is not counted): one tile ≥ 75 ms (noticed while it still renders) or two tiles ≥ 25 ms. After 16 quick tiles (< 10 ms) in a row the page counts as ordinary again |
| Previews of slow pages | A preview draws every object of the page, so on a slow page it waits until the sharp tiles on screen are done (an unfinished one is stopped) |
| Tile size | 512 px; 256 px on slow pages, so a dense area splits over several processes |
| How many helpers work | As many as the waiting slow work needs to finish in ~150 ms |
| Most renderers | min(CPU cores − 1, 4), the app included; 2 on battery; only the app when less than 1 GB of memory is free; and the helpers together about 256 MB (each holds its own copy of the parsed page, so on a large drawing fewer helpers run) |
| When helpers start | Once the first page is on screen (they must not slow down startup), or when slow work needs them |
| Priority | Below normal (Windows), so they never take the CPU from the UI |
| On standby | PDFium loaded, open documents open: ~9 MB each |
| Idle 5 s, battery, low memory | Stop. Stopping is what returns their memory: a process keeps memory PDFium freed |
| Crash | Only the helper dies; its tiles are rendered elsewhere. A page that crashed helpers twice is reported as failed |

The in-process engine takes any tile, but at most one slow tile at a time, so ordinary
tiles are never stuck behind slow ones. When a page turns out slow, the engine keeps only
the tile it is rendering and the others go back to the queue for the helpers.

### Editing (stages 6–9)

Helpers render from their own copy of the file, so edits must reach them. The plan: the
document the user edits lives only in the in-process engine; pages with unsaved edits are
rendered there only. After an edit, the pool hands the helpers a snapshot of the document
(saved to memory) to reopen in the background; until they have it, they only render pages
without edits.
