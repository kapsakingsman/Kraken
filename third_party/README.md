# Third-party code

## pdfium-render

[pdfium-render](https://github.com/ajrcarey/pdfium-render) 0.9.4 (crates.io release,
upstream commit `6cee8b9a`), used through `[patch.crates-io]` in the workspace
`Cargo.toml`. License: MIT or Apache-2.0 (`pdfium-render/LICENSE.md`).

Changes from the release:

- **Added `PdfPage::render_into_bitmap_cancellable`** (`src/pdf/document/page.rs`). It renders
  like `render_into_bitmap_with_config` with form data, but through PDFium's progressive API
  (`FPDF_RenderPageBitmap_Start` / `FPDF_RenderPage_Continue` / `FPDF_RenderPage_Close` with
  an `IFSDK_PAUSE` callback), so the engine can abandon a render nobody is waiting for any
  more. The release has no way to reach the page and bitmap handles this needs.
- Removed files not needed to build: bindings for PDFium versions other than 7881
  (`pdfium_latest`), the C headers used only by the `bindings` feature, and test PDFs.
- `src/lib.rs` allows all lints: vendored code is not held to our clippy settings, as with
  any crates.io dependency.

To update: copy the new release over this folder, delete the same files, and re-apply the
addition (search for "Kraken patch").
