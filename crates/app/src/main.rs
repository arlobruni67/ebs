//! EBS: gameplay recorder (Rust). `EBS.exe --probe-one <encoder>` is the child-process encoder probe.
#![windows_subsystem = "windows"]
mod app;
mod dialogs;
mod preview;
mod settings;
mod widgets;

use eframe::egui;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};

/// Milliseconds since start at the last UI frame; the watchdog logs when it stops advancing.
pub static UI_HEARTBEAT: AtomicU64 = AtomicU64::new(0);

pub fn now_ms() -> u64 {
    static START: OnceLock<std::time::Instant> = OnceLock::new();
    START.get_or_init(std::time::Instant::now).elapsed().as_millis() as u64
}

/// Logs native crashes (access violations etc.), which bypass the Rust panic hook.
unsafe extern "system" fn crash_filter(info: *const windows::Win32::System::Diagnostics::Debug::EXCEPTION_POINTERS) -> i32 {
    if let Some(rec) = info.as_ref().and_then(|i| i.ExceptionRecord.as_ref()) {
        let base = windows::Win32::System::LibraryLoader::GetModuleHandleW(None).map(|h| h.0 as usize).unwrap_or(0);
        let addr = rec.ExceptionAddress as usize;
        settings::log(format!(
            "CRASH: exception 0x{:08X} at 0x{addr:X} (exe base 0x{base:X}, offset 0x{:X}) on thread {:?}",
            rec.ExceptionCode.0 as u32,
            addr.wrapping_sub(base),
            std::thread::current().name().unwrap_or("?")
        ));
    }
    0 // EXCEPTION_CONTINUE_SEARCH: let Windows report it as usual.
}

/// Logs when the UI thread stops producing frames for more than 3 s ("Not Responding"), and when it recovers.
fn start_watchdog() {
    let _ = std::thread::Builder::new().name("ui-watchdog".into()).spawn(|| {
        let mut stalled_since: Option<u64> = None;
        loop {
            std::thread::sleep(std::time::Duration::from_millis(500));
            let beat = UI_HEARTBEAT.load(Ordering::Relaxed);
            if beat == 0 {
                continue;
            }
            let gap = now_ms().saturating_sub(beat);
            match stalled_since {
                None if gap > 3000 => {
                    stalled_since = Some(beat);
                    settings::log(format!("UI stalled: no frame for {gap} ms"));
                }
                Some(s) if beat != s => {
                    settings::log(format!("UI recovered after {} ms", beat.saturating_sub(s)));
                    stalled_since = None;
                }
                _ => {}
            }
        }
    });
}

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
    std::panic::set_hook(Box::new(|p| settings::log(format!("panic: {p}
{}", std::backtrace::Backtrace::force_capture()))));
    unsafe {
        windows::Win32::System::Diagnostics::Debug::SetUnhandledExceptionFilter(Some(crash_filter));
    }
    now_ms();
    start_watchdog();
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("EBS")
            .with_inner_size([1280.0, 860.0])
            .with_min_inner_size([980.0, 640.0])
            .with_icon(icon()),
        centered: true,
        ..Default::default()
    };
    let r = eframe::run_native(
        "EBS",
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
