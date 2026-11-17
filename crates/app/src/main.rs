//! BETTERBS: gameplay recorder (Rust). `BETTERBS.exe --probe-one <encoder>` is the child-process encoder probe.
#![windows_subsystem = "windows"]
mod app;
mod dialogs;
mod preview;
mod settings;
mod widgets;

use eframe::egui;
use std::sync::{Arc, OnceLock};

pub fn icon() -> Arc<egui::IconData> {
    static ICON: OnceLock<Arc<egui::IconData>> = OnceLock::new();
    ICON.get_or_init(|| Arc::new(egui::IconData { rgba: include_bytes!("../assets/icon64.rgba").to_vec(), width: 64, height: 64 })).clone()
}

fn probe_one(id: &str) -> i32 {
    let run = || -> anyhow::Result<String> {
        let adapters = ebs_engine::gpu::Gpu::enumerate_adapters()?;
        let a = ebs_engine::gpu::Gpu::choose_adapter(&adapters, None).ok_or_else(|| anyhow::anyhow!("no GPU"))?;
        let gpu = ebs_engine::gpu::Gpu::create(&a)?;
        Ok(serde_json::to_string(&ebs_engine::probe::probe_one(&gpu, id, 1920, 1080))?)
    };
    match run() {
        Ok(j) => {
            // The app is a GUI program: write the result to the inherited stdout pipe.
            use std::io::Write;
            let mut o = std::io::stdout();
            let _ = writeln!(o, "{j}");
            let _ = o.flush();
            0
        }
        Err(_) => 1,
    }
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() >= 3 && args[1] == "--probe-one" {
        std::process::exit(probe_one(&args[2]));
    }
    settings::log("main: start");
    std::panic::set_hook(Box::new(|p| settings::log(format!("panic: {p}"))));
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("BETTERBS")
            .with_inner_size([1280.0, 860.0])
            .with_min_inner_size([980.0, 640.0])
            .with_icon(icon()),
        centered: true,
        ..Default::default()
    };
    let r = eframe::run_native(
        "BETTERBS",
        options,
        Box::new(|cc| match { settings::log("main: creating app"); app::EbsApp::new(cc) } {
            Ok(a) => Ok(Box::new(a) as Box<dyn eframe::App>),
            Err(e) => {
                settings::log(format!("startup failed: {e:#}"));
                Err(e.into())
            }
        }),
    );
    if let Err(e) = r {
        settings::log(format!("eframe: {e}"));
    }
}
