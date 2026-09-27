//! Small values remembered between runs, stored as plain text files in the user's config
//! folder (`%APPDATA%\Kraken PDF` on Windows, `~/.config/kraken-pdf` elsewhere, or
//! `KRAKEN_CONFIG_DIR` if set). Losing them is harmless: they only speed up the next start.

use std::path::PathBuf;

const DISPLAY_SCALE_FILE: &str = "display-scale.txt";

fn config_dir() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os("KRAKEN_CONFIG_DIR") {
        return Some(PathBuf::from(dir));
    }
    if cfg!(windows) {
        return std::env::var_os("APPDATA").map(|dir| PathBuf::from(dir).join("Kraken PDF"));
    }
    std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")))
        .map(|dir| dir.join("kraken-pdf"))
}

/// The display scale (Windows "Scale" setting, 1.25 for 125%) the window had last time.
/// Known before the window exists, so the first page can be rendered at the right size
/// while the window is still being created.
pub fn load_display_scale() -> Option<f32> {
    let text = std::fs::read_to_string(config_dir()?.join(DISPLAY_SCALE_FILE)).ok()?;
    let scale: f32 = text.trim().parse().ok()?;
    (0.5..=8.0).contains(&scale).then_some(scale)
}

pub fn save_display_scale(scale: f32) {
    let Some(dir) = config_dir() else { return };
    // Best effort: without it the next start is only a little slower.
    let _ = std::fs::create_dir_all(&dir)
        .and_then(|()| std::fs::write(dir.join(DISPLAY_SCALE_FILE), scale.to_string()));
}
