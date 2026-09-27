# Roadmap

Rule: a stage starts only after the previous stage meets its "done when" check. Stages 1–4
are the core; smooth, sharp viewing has to work before any editing is added.

## UI rules for 144 Hz

At 144 Hz each frame has 6.9 ms. The UI thread should use under 3 ms of that.

- Framework: eframe/egui with the wgpu backend, `SurfaceConfig::LOW_LATENCY`
  (vsync follows the monitor's refresh rate, one frame of latency).
- The UI thread never calls PDFium. Everything goes through the engine thread.
- Upload at most about 4 tiles per frame. Tiles arrive as RGBA, so no pixel conversion happens
  on the UI thread.
- Scroll and zoom animations use elapsed time (`stable_dt`), so they move at the same speed at
  60 Hz and 144 Hz.
- Long lists (thumbnails, search results) only build the rows that are visible.
- Nothing repaints while the app is idle.
- Pass mark (the HUD's 8-second scroll test): under 1% missed frames, and UI CPU time under
  3 ms at the 99th percentile.

## Stages

- [x] **0. Setup:** Cargo workspace, pinned PDFium download scripts, CI on Windows and Linux.
  *Done when:* CI is green and `cargo run` works on the development PC.
- [x] **1. Engine and sharpness proof:** engine thread, tile rendering, stitch tests, and the
  `pdf-cli` tool for PNG output and benchmarks.
  *Done when:* the tiles-vs-full-page test passes and PNGs look as sharp as Acrobat at 200%.
  (Tests pass on Windows and Linux. The side-by-side Acrobat check was skipped for now and
  will be done in the app once stage 3 draws real pages.)
- [ ] **2. 144 Hz window:** eframe window, frame timing HUD with an 8-second scroll test,
  time-based smooth scrolling, open by Ctrl+O, drag-and-drop or command line.
  *Done when:* the scroll test passes on a 144 Hz monitor.
  (Built; waiting for the test result on the Windows PC.)
- [ ] **3. Continuous scroll:** page layout from page sizes, only visible pages built,
  low-resolution placeholders, LRU tile cache (~300 MB), per-frame upload budget.
  *Done when:* a 500-page PDF opens in under 1 s and scrolls at 144 fps with no blank flashes.
  (Built: previews, prefetching one screen above and below, 300 MB LRU tile cache, 4 uploads
  per frame. Tested: 500-page PDF opens in 4 ms, screen matches `pdf-cli render` pixel for
  pixel at 100% and 150% display scaling. Waiting for the scroll test on the Windows PC.)
- [ ] **4. Zoom:** Ctrl+wheel and pinch zoom around the cursor, GPU stretch during the gesture,
  exact re-render about 120 ms after it stops, old tiles kept until new ones arrive, stale
  requests dropped, progressive rendering for heavy pages, Fit Width / Fit Page / 100%.
  *Done when:* 25%–1600% is smooth, memory stays flat, with no flicker or seams.
- [ ] **5. Reader features:** text selection and copy, search, links, bookmarks, thumbnails,
  page jump, drag-and-drop, recent files, dark UI.
- [ ] **6. Annotations:** highlight, underline, strikeout, pen, shapes, notes and text boxes,
  drawn live on an overlay layer and committed on mouse-up. Undo/redo, form filling.
- [ ] **7. Save:** normal save (temp file, then rename). Incremental save for signed PDFs
  (`FPDF_SaveAsCopy` with `FPDF_INCREMENTAL`), with a warning when editing a signed file.
  *Done when:* saved files open in Acrobat and existing signatures stay intact.
- [ ] **8. Visual signature:** draw, type or import a signature, then place and resize it.
- [ ] **9. Digital signature:** certificate signing as the last step, using `pdf_signer` or a
  pyHanko helper process, whichever passes Acrobat verification.
- [ ] **10. Packaging:** Windows installer with `pdfium.dll` and its licenses, `.pdf` file
  association, crash log.

## Verified facts behind the design

- Tiles are rendered with `set_origin` (page drawn at full size, shifted). The matrix path
  drops form fields and cannot use progressive rendering. A tile matrix must be
  `[s, 0, 0, s, -x, -y]`; flipping `d` renders the page upside down.
- Saving without `FPDF_INCREMENTAL` breaks existing signatures. `pdfium-render` does not
  expose incremental save, so Stage 7 calls the raw binding.
- For annotations, `FPDFPage_GenerateContent` is not needed: annotations live in `/Annots`,
  not in the page's content stream.
