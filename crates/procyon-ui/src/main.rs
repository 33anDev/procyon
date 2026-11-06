//! Procyon — a transparent, open-source process monitor.
//!
//! Entry point. Sets up the native window and hands control to `ProcyonApp`.

// On Windows, don't spawn a console window alongside the GUI in release builds.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;
mod process;
mod theme;

use app::ProcyonApp;

fn main() -> eframe::Result<()> {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([960.0, 620.0])
            .with_min_inner_size([560.0, 360.0])
            .with_title("Procyon"),
        ..Default::default()
    };

    eframe::run_native(
        "Procyon",
        options,
        Box::new(|cc| Ok(Box::new(ProcyonApp::new(cc)))),
    )
}