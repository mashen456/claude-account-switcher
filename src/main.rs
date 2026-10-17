// Im Release-Build ohne Konsolenfenster starten.
#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

#[macro_use]
mod i18n;

mod app;
mod claude;
mod cli;
mod clipboard;
mod fsx;
mod locks;
mod login;
mod settings;
mod slots;
mod store;
mod usage;

use eframe::egui;

fn main() -> eframe::Result {
    // Gewählte Sprache, sonst die des Systems.
    let saved = slots::SlotStore::open().ok().and_then(|store| store.settings().language);
    i18n::set(saved.unwrap_or_else(i18n::system));

    let args: Vec<String> = std::env::args().skip(1).collect();
    if !args.is_empty() {
        std::process::exit(cli::run(&args));
    }

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("Claude Slots")
            .with_inner_size([460.0, 600.0])
            .with_min_inner_size([380.0, 420.0]),
        ..Default::default()
    };
    eframe::run_native(
        "Claude Slots",
        options,
        Box::new(|cc| Ok(Box::new(app::App::new(cc)))),
    )
}
