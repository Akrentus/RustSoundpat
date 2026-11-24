mod app;
mod audio;
mod config;
mod hotkeys;
mod icons;
mod model;
mod pipewire;
mod recorder;

use crate::app::SoundpadApp;
use crate::config::load_config;
use crate::pipewire::PipewireVirtualMic;
use eframe::egui;

fn main() -> eframe::Result<()> {
    let config = load_config();
    let pipewire = PipewireVirtualMic::new();
    let cleanup_handle = pipewire.cleanup_handle();

    // Ensure SIGINT / SIGTERM cleanly unloads only the pactl modules created by this app
    let _ = ctrlc::set_handler(move || {
        cleanup_handle.unload_all();
        std::process::exit(0);
    });

    let icon = eframe::icon_data::from_png_bytes(include_bytes!("../assets/icon.png"))
        .expect("Failed to load embedded application icon");

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("SoundPat")
            .with_app_id("soundpat")
            .with_icon(icon)
            .with_inner_size([960.0, 580.0])
            .with_min_inner_size([680.0, 420.0])
            .with_drag_and_drop(true),
        ..Default::default()
    };

    eframe::run_native(
        "SoundPat",
        options,
        Box::new(move |cc| Ok(Box::new(SoundpadApp::new(cc, config, pipewire)))),
    )
}
