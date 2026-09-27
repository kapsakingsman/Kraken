// Release builds are GUI apps on Windows: no console window behind them.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;
#[cfg(feature = "automation")]
mod automation;
mod display;
mod hud;
mod settings;
mod startup;
#[cfg(feature = "automation")]
mod threads;
mod tiles;

use std::path::PathBuf;
use std::sync::{Arc, OnceLock};

use eframe::egui;

fn main() -> eframe::Result {
    // A render worker (see pdf_engine::RenderPool): PDFium only, no window.
    if std::env::args_os().nth(1).as_deref() == Some(std::ffi::OsStr::new(startup::WORKER_FLAG)) {
        std::process::exit(match pdf_engine::run_worker() {
            Ok(()) => 0,
            Err(_) => 1,
        });
    }
    #[cfg(feature = "automation")]
    automation::mark("main");
    let path = std::env::args_os().nth(1).map(PathBuf::from);
    // PDFium starts and renders the first page while the window and GPU are set up.
    let display_scale = settings::load_display_scale();
    let repaint = Arc::new(OnceLock::new());
    let boot = startup::begin(path, display_scale, Arc::clone(&repaint));
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
            let _ = repaint.set(cc.egui_ctx.clone());
            let boot = boot.join().unwrap_or_else(|_| startup::Boot {
                engine: Err("starting PDFium failed".into()),
                opening: None,
            });
            let gpu = gpu_description(cc);
            Ok(Box::new(app::ViewerApp::new(cc, boot, gpu, display_scale)))
        }),
    )
}

/// Which GPU and graphics API draw the window, e.g. "NVIDIA GeForce RTX 3060
/// (discrete GPU, Dx12)". A "CPU" device type means software rendering (Microsoft Basic
/// Render Driver / WARP, or llvmpipe): no hardware acceleration.
pub fn gpu_description(cc: &eframe::CreationContext) -> String {
    let Some(state) = &cc.wgpu_render_state else {
        return "unknown".into();
    };
    let info = state.adapter.get_info();
    let kind = match info.device_type {
        eframe::wgpu::DeviceType::DiscreteGpu => "discrete GPU",
        eframe::wgpu::DeviceType::IntegratedGpu => "integrated GPU",
        eframe::wgpu::DeviceType::VirtualGpu => "virtual GPU",
        eframe::wgpu::DeviceType::Cpu => "SOFTWARE, no GPU acceleration",
        eframe::wgpu::DeviceType::Other => "other",
    };
    format!("{} ({kind}, {:?})", info.name, info.backend)
}

/// Also asks the GPU memory allocator to favour low memory use.
///
/// On Windows, use only Direct3D 12. By default wgpu also sets up Vulkan and OpenGL to pick
/// the best, which on the development PC cost 129 ms of every start and 36 MB of memory
/// (perf-runner startup: 493 -> 386 ms to the first sharp page). Every Windows 10/11 PC has
/// Direct3D 12, with a software fallback when there is no GPU. `WGPU_BACKEND` still wins.
fn gpu_setup() -> eframe::egui_wgpu::WgpuSetup {
    let mut setup = eframe::egui_wgpu::WgpuSetupCreateNew::without_display_handle();
    // Smaller GPU memory blocks: the app draws a few hundred textures at most, so the
    // allocator's large up-front blocks (made for games) only cost memory.
    let device_descriptor = std::sync::Arc::clone(&setup.device_descriptor);
    setup.device_descriptor = std::sync::Arc::new(move |adapter| {
        let mut descriptor = device_descriptor(adapter);
        descriptor.memory_hints = eframe::wgpu::MemoryHints::MemoryUsage;
        descriptor
    });
    if cfg!(windows) && eframe::wgpu::Backends::from_env().is_none() {
        setup.instance_descriptor.backends = eframe::wgpu::Backends::DX12;
    }
    setup.into()
}
