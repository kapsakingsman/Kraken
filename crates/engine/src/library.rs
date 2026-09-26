use std::path::{Path, PathBuf};

use pdfium_render::prelude::Pdfium;

use crate::EngineError;

/// Finds the folder that contains the PDFium shared library (`pdfium.dll` on Windows).
///
/// If the `PDFIUM_LIB_DIR` environment variable is set, only that folder is used. Otherwise
/// the search order is:
/// 1. the folder of the running executable (how the installed app ships it),
/// 2. `vendor/pdfium`, filled by `scripts/fetch-pdfium` (development builds).
pub fn locate_pdfium() -> Result<PathBuf, EngineError> {
    let mut candidates = Vec::new();
    if let Some(dir) = std::env::var_os("PDFIUM_LIB_DIR") {
        candidates.push(PathBuf::from(dir));
    } else {
        add_default_locations(&mut candidates);
    }

    if let Some(dir) = candidates
        .iter()
        .find(|dir| Pdfium::pdfium_platform_library_name_at_path(dir).is_file())
    {
        return Ok(dir.clone());
    }
    Err(EngineError::LibraryNotFound {
        searched: candidates
            .iter()
            .map(|d| d.display().to_string())
            .collect::<Vec<_>>()
            .join(", "),
    })
}

fn add_default_locations(candidates: &mut Vec<PathBuf>) {
    if let Some(dir) = std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(Path::to_path_buf))
    {
        candidates.push(dir);
    }
    // Joined piece by piece so Windows gets backslashes; LoadLibrary rejects some '/' paths.
    let vendor = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("vendor")
        .join("pdfium");
    candidates.push(vendor.join(if cfg!(windows) { "bin" } else { "lib" }));
}
