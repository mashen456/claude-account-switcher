// Im Release-Build ohne Konsolenfenster starten.
#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

mod app;
mod claude;
mod cli;
mod clipboard;
mod fsx;
mod locks;
mod slots;
mod store;
mod usage;

use eframe::egui;

fn main() -> eframe::Result {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if !args.is_empty() {
        std::process::exit(cli::run(&args));
    }

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("Claude Slots")
            .with_inner_size([460.0, 560.0])
            .with_min_inner_size([380.0, 420.0]),
        ..Default::default()
    };
    eframe::run_native(
        "Claude Slots",
        options,
        Box::new(|cc| Ok(Box::new(app::App::new(cc)))),
    )
}
