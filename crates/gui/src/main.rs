//! The Kitten desktop app. It never touches a disk: it asks `bootshiftd` over the system
//! D-Bus, and falls back to a made-up demo disk when the helper isn't there (or with `--demo`),
//! saying so in the rail on every screen.

mod app;
mod backend;
mod demo;
mod disk_bar;
mod editing;
mod format;
mod grid;
mod gutter;
mod migrate;
mod motion;
mod protocol;
mod readout;
mod save;
mod screens;
mod shell;
mod table;
mod theme;
mod widgets;

#[cfg(test)]
mod ui_tests;

use anyhow::Context;
use eframe::egui;

use crate::backend::{Backend, HelperBackend, Worker};
use crate::demo::DemoBackend;

fn main() -> anyhow::Result<()> {
    let backend = choose_backend(std::env::args().any(|arg| arg == "--demo"));
    // The migration demo runs on the box's own 1366x768 screen; a smaller window fits it whole.
    let size = if std::env::var_os("BOOTSHIFT_MIGRATE_DEMO").is_some() {
        [1120.0, 720.0]
    } else {
        [1440.0, 900.0]
    };
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("Kitten")
            .with_app_id("org.bootshift.Kitten")
            .with_inner_size(size)
            .with_min_inner_size([720.0, 560.0]),
        ..eframe::NativeOptions::default()
    };
    eframe::run_native(
        "Kitten",
        options,
        Box::new(|context| {
            let wake = context.egui_ctx.clone();
            let worker = Worker::spawn(backend, move || wake.request_repaint());
            Ok(Box::new(app::App::new(worker)))
        }),
    )
    .map_err(|error| anyhow::anyhow!("{error}"))
    .context("opening the Kitten window")
}

fn choose_backend(demo: bool) -> Box<dyn Backend> {
    if demo {
        return Box::new(DemoBackend::new());
    }
    match HelperBackend::connect() {
        Ok(helper) => Box::new(helper),
        Err(error) => {
            eprintln!(
                "bootshift-gui: the helper service isn't available ({}), showing the demo disk",
                error.0
            );
            Box::new(DemoBackend::new())
        }
    }
}
