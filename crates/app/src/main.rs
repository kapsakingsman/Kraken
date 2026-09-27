// Release builds are GUI apps on Windows: no console window behind them.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;
#[cfg(feature = "automation")]
mod automation;
mod hud;
mod tiles;

use std::path::PathBuf;

use eframe::egui;

fn main() -> eframe::Result {
    #[cfg(feature = "automation")]
    automation::mark_main_started();
    let path = std::env::args_os().nth(1).map(PathBuf::from);
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("Kraken PDF")
            .with_inner_size([1100.0, 860.0])
            .with_min_inner_size([480.0, 360.0])
            .with_drag_and_drop(true),
        renderer: eframe::Renderer::Wgpu,
        wgpu_options: eframe::WgpuConfiguration {
            // Vsync at the monitor's own refresh rate (60, 144, ...) with only one frame
            // queued, so input shows up on screen as early as possible.
            surface: eframe::SurfaceConfig::LOW_LATENCY,
            ..Default::default()
        },
        ..Default::default()
    };
    eframe::run_native(
        "Kraken PDF",
        options,
        Box::new(|cc| Ok(Box::new(app::ViewerApp::new(cc, path)))),
    )
}
