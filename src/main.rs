// No console window behind the GUI.
#![windows_subsystem = "windows"]

mod app;
mod claude;
mod cmd;
mod config;
mod form;
mod github;
mod jobs;
mod ops;
mod project;
mod tasks;

use eframe::egui;

fn main() -> eframe::Result {
    let viewport = egui::ViewportBuilder::default()
        .with_title("GitHub Uploader")
        .with_icon(app::icon())
        .with_inner_size([1320.0, 880.0])
        .with_min_inner_size([960.0, 620.0]);
    eframe::run_native(
        "GitHub Uploader",
        eframe::NativeOptions {
            viewport,
            ..Default::default()
        },
        Box::new(|cc| Ok(Box::new(app::App::new(cc)))),
    )
}
