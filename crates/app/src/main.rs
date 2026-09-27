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
    automation::mark("main");
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
            wgpu_setup: gpu_setup(),
            ..Default::default()
        },
        ..Default::default()
    };
    eframe::run_native(
        "Kraken PDF",
        options,
        Box::new(|cc| {
            // eframe has created the window, the GPU device and the fonts by now.
            #[cfg(feature = "automation")]
            automation::mark("window_and_gpu");
            Ok(Box::new(app::ViewerApp::new(cc, path)))
        }),
    )
}

/// On Windows, use only Direct3D 12. By default wgpu also sets up Vulkan and OpenGL to pick
/// the best, which on the development PC cost 129 ms of every start and 36 MB of memory
/// (perf-runner startup: 493 -> 386 ms to the first sharp page). Every Windows 10/11 PC has
/// Direct3D 12, with a software fallback when there is no GPU. `WGPU_BACKEND` still wins.
fn gpu_setup() -> eframe::egui_wgpu::WgpuSetup {
    let mut setup = eframe::egui_wgpu::WgpuSetupCreateNew::without_display_handle();
    if cfg!(windows) && eframe::wgpu::Backends::from_env().is_none() {
        setup.instance_descriptor.backends = eframe::wgpu::Backends::DX12;
    }
    setup.into()
}
