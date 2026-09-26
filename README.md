# Kraken PDF

A fast PDF reader and editor for Windows, written in Rust on top of PDFium.

Goals, in order: Acrobat-level sharp rendering, smooth scrolling and zooming at up to 144 Hz,
then annotations, forms and signing. See [docs/ROADMAP.md](docs/ROADMAP.md) for the stages.

**Current stage:** 1, the rendering engine and a command-line tool. There is no window yet.

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
