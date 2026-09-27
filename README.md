# Kraken PDF

A fast PDF reader and editor for Windows, written in Rust on top of PDFium.

Goals, in order: Acrobat-level sharp rendering, smooth scrolling and zooming at up to 144 Hz,
then annotations, forms and signing. See [docs/ROADMAP.md](docs/ROADMAP.md) for the stages.

**Current stage:** 4, zooming (10%–1600%) and continuous scrolling through real page content
at up to 144 Hz.

## Setup (Windows 11)

1. Install Rust from <https://rustup.rs> with the default settings. If it offers to install the
   Visual Studio Build Tools, accept: Rust needs them to link programs on Windows.
2. Get the code and download PDFium:

   ```powershell
   git clone https://github.com/kapsakingsman/Kraken
   cd Kraken
   powershell -ExecutionPolicy Bypass -File scripts\fetch-pdfium.ps1
   ```

3. Run the tests:

   ```powershell
   cargo test --workspace
   ```

## The app

```powershell
cargo run --release -p kraken-pdf -- "C:\path\to\file.pdf"
```

Without a file it shows a 200-page demo layout. Open files with **Ctrl+O** or by dropping them
on the window.

| Input | Action |
|---|---|
| Mouse wheel, touchpad | Scroll (wheel steps are animated, touchpad follows your fingers) |
| Ctrl+wheel, touchpad pinch | Zoom around the mouse pointer |
| Ctrl+plus / Ctrl+minus | Next / previous zoom step |
| Ctrl+0 / Ctrl+1 / Ctrl+2 | Fit page / 100% / fit width |
| Shift+wheel | Scroll sideways (when zoomed in) |
| Arrow keys, Page Up/Down, Space, Shift+Space | Scroll |
| Home / End | First / last page |
| Scrollbar | Drag the thumb, or click the track |
| F3 | Show or hide the frame timing HUD (also shows tile cache use) |

During a zoom gesture the existing tiles are stretched on the GPU, so zooming never waits for
rendering; 120 ms after the zoom stops, sharp tiles for the new zoom replace them. While a
page's sharp tiles render, a low-resolution preview of it is shown, so pages never
flash blank during normal scrolling. Tiles are rendered at your screen's real pixel density
and drawn 1:1, so the page on screen is pixel-for-pixel what `pdf-cli render` produces.

### Checking 144 Hz smoothness

1. Set your monitor to 144 Hz: Settings > System > Display > Advanced display > Choose a
   refresh rate.
2. Start the app with `--release` (debug builds are much slower).
3. Click **Run 8-second scroll test** in the HUD and wait for the result.
4. PASS means under 1% missed frames and UI CPU time under 3 ms at the 99th percentile.

## Command-line tool

```powershell
# Page count and sizes
cargo run --release -p pdf-cli -- info C:\path\to\file.pdf

# Render page 3 at 200% to a PNG in the out\ folder
cargo run --release -p pdf-cli -- render C:\path\to\file.pdf --page 3 --zoom 200 --display-scale 1.5

# How fast is each page and tile?
cargo run --release -p pdf-cli -- bench C:\path\to\file.pdf --zoom 150 --display-scale 1.5
```

- `--zoom` is the same number as Acrobat's zoom box.
- `--display-scale` is your Windows display scaling (Settings > System > Display > Scale):
  `1.25` for 125%, `1.5` for 150%, and so on.
- `--password` opens protected files.

The PNG contains exactly the pixels the app will show at that zoom, because it is rendered through
the same tile pipeline.

### Comparing with Acrobat

1. In Acrobat, open Edit > Preferences > Page Display and set Resolution to "Use system setting".
2. Set Acrobat's zoom to 200% and take a screenshot of part of the page (Win+Shift+S).
3. Run `render` with `--zoom 200` and your display scale, then open the PNG at 100% (actual size,
   not "fit to window") and compare the same area.

## Project layout

| Path | What it is |
|---|---|
| `crates/engine` | Owns PDFium on one dedicated thread, opens documents and renders 512×512 tiles |
| `crates/view` | UI-independent viewer logic: page layout, smooth scrolling, frame statistics |
| `crates/app` | `kraken-pdf`, the desktop app (eframe/egui on wgpu) |
| `crates/cli` | `pdf-cli`: `info`, `render` and `bench` commands |
| `scripts/` | Downloads PDFium. The version and SHA-256 checksums are pinned in `scripts/pdfium.lock` |
| `vendor/pdfium` | Downloaded PDFium (not committed) |

## PDFium

- Build 8066 from [bblanchon/pdfium-binaries](https://github.com/bblanchon/pdfium-binaries),
  loaded at runtime (`pdfium.dll`).
- Rust bindings: [pdfium-render](https://github.com/ajrcarey/pdfium-render) 0.9.4. Its newest
  API level is 7881; build 8066 loads with it and passes all tests.
- To upgrade: change `version` and the checksums in `scripts/pdfium.lock`, delete
  `vendor\pdfium`, run the fetch script again, and run the tests.
- PDFium's own licenses are in `vendor/pdfium/licenses`. They must ship with the app.

## Known build issue

If a `cargo update` brings back Windows build errors in `wgpu-hal` about two versions of the
`windows` crate, pin them to the same version again:

```powershell
cargo update -p windows@0.61.3 --precise 0.62.2
```
