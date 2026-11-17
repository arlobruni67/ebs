//! The main window: top bar, preview, dock (scenes, sources, audio mixer, controls) and status bar.
use crate::dialogs::{self, AddSourceState, BenchState, SettingsState};
use crate::preview::{PreviewEditor, PreviewEvent, PreviewWindow};
use crate::settings::{self, log, AppSettings, VideoMode};
use crate::widgets::{self as w, FIELD};
use eframe::egui::{self, Color32, RichText, Sense, Vec2};
use ebs_engine::audio::{list_devices, AudioSource};
use ebs_engine::collection::{fit_for, safe_name, SceneCollection};
use ebs_engine::encoding::{self, QualityPreset, RateControl};
use ebs_engine::monitor::SystemMonitor;
use ebs_engine::probe::ProbeResult;
use ebs_engine::recorder::{Recorder, RecordingResult};
use ebs_engine::scene::Scene;
use ebs_engine::sources::VideoSource;
use ebs_engine::{Engine, TransitionKind, VideoCanvas};
use raw_window_handle::{HasWindowHandle, RawWindowHandle};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::sync::mpsc::{channel, Receiver};
use std::sync::Arc;
use std::time::{Duration, Instant};

pub const BG: Color32 = Color32::from_rgb(0x17, 0x18, 0x1B);
pub const PANEL: Color32 = Color32::from_rgb(0x1F, 0x21, 0x25);
pub const PANEL_HI: Color32 = Color32::from_rgb(0x27, 0x2A, 0x2F);
pub const LINE: Color32 = Color32::from_rgb(0x33, 0x36, 0x3C);
pub const TEXT: Color32 = Color32::from_rgb(0xDC, 0xDE, 0xE3);
pub const MUTED: Color32 = Color32::from_rgb(0x8A, 0x8F, 0x98);
pub const ACCENT: Color32 = Color32::from_rgb(0x3D, 0x7E, 0xEA);
pub const ICON: Color32 = TEXT;
pub const SELECT: Color32 = Color32::from_rgb(0x24, 0x40, 0x6E);
pub const REC: Color32 = Color32::from_rgb(0xE0, 0x39, 0x3E);
pub const WARN: Color32 = Color32::from_rgb(0xE5, 0xA2, 0x3A);
pub const GOOD: Color32 = Color32::from_rgb(0x4F, 0xBF, 0x6B);

/// Segoe MDL2 Assets glyphs (the same icons the C# EBS used), drawn with the "icons" font family.
pub mod icon {
    pub const ADD: &str = "\u{E710}";
    pub const REMOVE: &str = "\u{E738}";
    pub const RENAME: &str = "\u{E8AC}";
    pub const COPY: &str = "\u{E8C8}";
    pub const UP: &str = "\u{E70E}";
    pub const DOWN: &str = "\u{E70D}";
    pub const FIT: &str = "\u{E740}";
    pub const TRANSFORM: &str = "\u{E7AD}";
    pub const SETTINGS: &str = "\u{E713}";
    pub const OPTIMISE: &str = "\u{EC4A}";
    pub const FOLDER: &str = "\u{E838}";
    pub const CAMERA: &str = "\u{E722}";
    pub const EYE: &str = "\u{E7B3}";
    pub const EYE_OFF: &str = "\u{ED1A}";
    pub const LOCK: &str = "\u{E72E}";
    pub const UNLOCK: &str = "\u{E785}";
    pub const VOLUME: &str = "\u{E767}";
    pub const MUTE: &str = "\u{E74F}";
    pub const PAUSE: &str = "\u{E769}";
    pub const PLAY: &str = "\u{E768}";
    pub const GAME: &str = "\u{E7FC}";
    pub const WINDOW: &str = "\u{E737}";
    pub const DISPLAY: &str = "\u{E7F4}";
    pub const SCENE: &str = "\u{E8A9}";
    pub const PATTERN: &str = "\u{E91B}";
}

pub fn icon_text(glyph: &str) -> RichText {
    RichText::new(glyph).family(egui::FontFamily::Name("icons".into())).color(ICON)
}

pub enum PromptAction {
    AddScene,
    RenameScene(u64),
    RenameSource(u64),
    RenameAudio(u64),
    NewCollection,
    DuplicateCollection,
    RenameCollection,
}

pub enum ConfirmAction {
    RemoveScene(u64),
    RemoveSource(u64),
    RemoveAudio(u64),
    DeleteCollection,
    RecordLowDisk,
    StopAndClose,
}

pub enum Dialog {
    Prompt { title: String, label: String, text: String, action: PromptAction },
    Confirm { title: String, message: String, ok: String, action: ConfirmAction },
    Message { title: String, message: String },
    AddSource(Box<AddSourceState>),
    Transform { item: u64, fields: dialogs::TransformFields },
    Settings(Box<SettingsState>),
    Benchmark(Box<BenchState>),
}

enum RecState {
    Idle,
    Starting(Receiver<anyhow::Result<Arc<Recorder>>>),
    Recording(Arc<Recorder>),
    Stopping(Arc<Recorder>, Receiver<anyhow::Result<RecordingResult>>),
}

#[derive(Default)]
struct Stats {
    perf: String,
    disk: String,
    last_bytes: i64,
    mbps: f64,
    cap_source: Option<u64>,
    cap_frames: u64,
}

pub struct EbsApp {
    pub s: AppSettings,
    pub engine: Engine,
    pub coll: SceneCollection,
    preview: Option<PreviewWindow>,
    editor: PreviewEditor,
    pub selected: Option<u64>,
    rec: RecState,
    probe_rx: Option<Receiver<Vec<ProbeResult>>>,
    pub probe_ready: bool,
    probe_status: Option<String>,
    monitor: SystemMonitor,
    last_stats: Instant,
    stats: Stats,
    meters: HashMap<u64, (f32, f32)>,
    pub dialog: Option<Dialog>,
    popup_open: bool,
    last_file: Option<PathBuf>,
    last_file_text: String,
    rec_info: String,
    close_after_stop: bool,
    allow_close: bool,
    transition_ms_text: String,
    /// Open dialog's viewport and its screen position (fixed when it opens).
    pub dlg_vp: Option<(egui::ViewportId, egui::Pos2)>,
    pub main_focused_last: bool,
}

fn exe_args_probe() -> Vec<&'static str> {
    vec!["--probe-one"]
}

impl EbsApp {
    pub fn new(cc: &eframe::CreationContext<'_>) -> anyhow::Result<EbsApp> {
        setup_style(&cc.egui_ctx);
        log("new: style ok");
        let mut s = AppSettings::load();
        if !s.imported {
            s.import_from_csharp();
            s.save();
        }
        log(format!("app started (pid {})", std::process::id()));
        let engine = Engine::new(VideoCanvas::new(s.canvas_width, s.canvas_height, s.fps_num, s.fps_den), None)?;
        engine.preview_divisor.store(s.preview_fps_divisor.max(1), Ordering::Relaxed);
        engine.preview_enabled.store(s.preview_enabled, Ordering::Relaxed);

        log("new: engine ok");
        let mut coll = SceneCollection::new(&settings::scenes_dir(), &s.collection);
        let mut warnings = Vec::new();
        let loaded = coll.load(&engine.shared, &s.collection.clone(), &mut |w| warnings.push(w)).unwrap_or_else(|e| {
            warnings.push(format!("Saved scenes could not be loaded ({e}); started with the default setup."));
            false
        });
        if !loaded {
            coll.unload(&engine.shared);
            coll.name = s.collection.clone();
            coll.create_default(&engine.shared);
        }
        for w in &warnings {
            log(w);
        }

        log("new: scenes ok");
        let preview = match cc.window_handle().map(|h| h.as_raw()) {
            Ok(RawWindowHandle::Win32(h)) => match PreviewWindow::create(h.hwnd.get()) {
                Ok(p) => {
                    if let Err(e) = engine.attach_preview(p.hwnd, 16, 16) {
                        log(format!("preview attach failed: {e}"));
                    }
                    Some(p)
                }
                Err(e) => {
                    log(format!("preview window failed: {e}"));
                    None
                }
            },
            _ => None,
        };

        let mut app = EbsApp {
            transition_ms_text: coll.transition_ms.to_string(),
            s,
            engine,
            coll,
            preview,
            editor: PreviewEditor::default(),
            selected: None,
            rec: RecState::Idle,
            probe_rx: None,
            probe_ready: false,
            probe_status: None,
            monitor: SystemMonitor::new(),
            last_stats: Instant::now() - Duration::from_secs(2),
            stats: Stats::default(),
            meters: HashMap::new(),
            dialog: None,
            popup_open: false,
            last_file: None,
            last_file_text: String::new(),
            rec_info: String::new(),
            close_after_stop: false,
            allow_close: false,
            dlg_vp: None,
            main_focused_last: false,
        };
        if !warnings.is_empty() {
            app.dialog = Some(Dialog::Message { title: "BETTERBS".into(), message: warnings.join("\n") });
        }
        app.save_scenes();
        app.start_probe(false);
        Ok(app)
    }

    // ------------------------------------------------------------------ encoder probe

    /// Runs the encoder probe in child processes (a driver crash can't take the app down). Cached per driver.
    pub fn start_probe(&mut self, force: bool) {
        if !force && self.s.probe_valid() {
            self.after_probe();
            return;
        }
        self.probe_ready = false;
        self.probe_status = Some("Checking encoders on this PC...".into());
        let (tx, rx) = channel();
        std::thread::spawn(move || {
            let exe = std::env::current_exe().unwrap_or_default();
            let args = exe_args_probe();
            let results: Vec<ProbeResult> = encoding::CATALOG.iter().map(|e| ebs_engine::probe::run_isolated(&exe, &args, e.id, Duration::from_secs(60))).collect();
            let _ = tx.send(results);
        });
        self.probe_rx = Some(rx);
    }

    fn after_probe(&mut self) {
        let working = self.s.working_encoders();
        if !working.iter().any(|e| e.id == self.s.video.encoder_id) {
            // Best first: Turing NVENC HEVC, then H.264, then AMD, then CPU.
            if let Some(pick) = ["nvenc_hevc", "nvenc_h264", "amf_hevc", "amf_h264", "x264"].iter().find(|id| working.iter().any(|w| w.id == **id)) {
                self.s.video.encoder_id = pick.to_string();
                self.s.video.preset = Some(encoding::get(pick).default_preset.into());
                self.s.save();
            }
        }
        self.probe_ready = !working.is_empty();
        self.probe_status = (!self.probe_ready).then(|| "No working video encoder found (see ebs.log)".into());
    }

    fn poll_probe(&mut self) {
        let Some(rx) = &self.probe_rx else { return };
        if let Ok(r) = rx.try_recv() {
            for p in &r {
                log(format!("probe {}: works {} features {:#x} {}", p.encoder_id, p.works, p.verified_features, p.note));
            }
            self.s.probe = r;
            self.s.probe_fingerprint = Some(settings::hardware_fingerprint());
            self.s.save();
            self.probe_rx = None;
            self.after_probe();
        }
    }

    // ------------------------------------------------------------------ helpers

    pub fn save_scenes(&self) {
        if let Err(e) = self.coll.save(&self.engine.shared) {
            log(format!("scene save failed: {e}"));
        }
    }

    pub fn is_recording(&self) -> bool {
        !matches!(self.rec, RecState::Idle)
    }

    fn message(&mut self, title: &str, msg: impl Into<String>) {
        self.dialog = Some(Dialog::Message { title: title.into(), message: msg.into() });
    }

    fn prompt(&mut self, title: &str, label: &str, text: &str, action: PromptAction) {
        self.dialog = Some(Dialog::Prompt { title: title.into(), label: label.into(), text: text.into(), action });
    }

    fn confirm(&mut self, title: &str, msg: impl Into<String>, ok: &str, action: ConfirmAction) {
        self.dialog = Some(Dialog::Confirm { title: title.into(), message: msg.into(), ok: ok.into(), action });
    }

    fn current(&self) -> Arc<Scene> {
        self.engine.current_scene()
    }

    fn scene_by_id(&self, id: u64) -> Option<Arc<Scene>> {
        self.coll.scenes.iter().find(|s| s.id == id).cloned()
    }

    fn unique_scene_name(&self, base: &str) -> String {
        let mut name = base.to_string();
        let mut i = 2;
        while self.coll.scenes.iter().any(|s| s.name() == name) {
            name = format!("{base} {i}");
            i += 1;
        }
        name
    }

    fn unique_collection_name(&self, base: &str) -> String {
        let existing: Vec<String> = SceneCollection::list(&self.coll.dir).iter().map(|s| s.to_lowercase()).collect();
        let mut name = base.to_string();
        let mut i = 2;
        while existing.contains(&name.to_lowercase()) {
            name = format!("{base} {i}");
            i += 1;
        }
        name
    }

    fn select(&mut self, id: Option<u64>) {
        self.selected = id;
        *self.engine.preview_selection.lock().unwrap() = id;
    }

    /// Stops a capture when no scene uses it any more.
    fn release_if_unused(&self, source_id: u64) {
        if self.coll.scenes.iter().any(|s| s.snapshot().iter().any(|i| i.source.id() == source_id)) {
            return;
        }
        self.engine.remove_source(source_id);
    }

    // ------------------------------------------------------------------ actions

    fn switch_scene(&mut self, s: Arc<Scene>) {
        if Arc::ptr_eq(&s, &self.current()) {
            return;
        }
        self.engine.switch_scene(s, self.coll.transition, self.coll.transition_ms);
        self.select(None);
        self.save_scenes();
    }

    fn move_scene(&mut self, d: i32) {
        let cur = self.current();
        let Some(i) = self.coll.scenes.iter().position(|s| Arc::ptr_eq(s, &cur)) else { return };
        let j = (i as i32 + d).clamp(0, self.coll.scenes.len() as i32 - 1) as usize;
        if i != j {
            let s = self.coll.scenes.remove(i);
            self.coll.scenes.insert(j, s);
            self.save_scenes();
        }
    }

    fn duplicate_scene(&mut self) {
        let s = self.current();
        let copy = Scene::new(&self.unique_scene_name(&s.name()));
        for it in s.snapshot() {
            let mut n = it.clone();
            n.id = ebs_engine::scene::next_id();
            copy.add_item(n);
        }
        let i = self.coll.scenes.iter().position(|x| Arc::ptr_eq(x, &s)).unwrap_or(0);
        self.coll.scenes.insert(i + 1, copy.clone());
        self.engine.set_current_scene(copy);
        self.select(None);
        self.save_scenes();
    }

    fn remove_scene(&mut self, id: u64) {
        let Some(s) = self.scene_by_id(id) else { return };
        let idx = self.coll.scenes.iter().position(|x| x.id == id).unwrap_or(0);
        self.coll.scenes.retain(|x| x.id != id);
        let next = self.coll.scenes[idx.min(self.coll.scenes.len() - 1)].clone();
        self.engine.set_current_scene(next);
        // Drop it from every scene that shows it as a nested scene.
        for src in self.engine.sources() {
            if let Some(ss) = src.as_scene_source() {
                if ss.inner().is_some_and(|x| x.id == id) {
                    self.coll.remove_source_everywhere(&self.engine.shared, src.id());
                }
            }
        }
        for it in s.snapshot() {
            self.release_if_unused(it.source.id());
        }
        self.select(None);
        self.save_scenes();
    }

    fn remove_item(&mut self, item: u64) {
        let scene = self.current();
        let Some(it) = scene.item(item) else { return };
        scene.remove(item);
        self.select(None);
        self.release_if_unused(it.source.id());
        self.save_scenes();
    }

    fn fit_item(&mut self, item: u64) {
        let c = self.engine.canvas();
        self.current().update(item, |it| {
            it.crop_left = 0;
            it.crop_top = 0;
            it.crop_right = 0;
            it.crop_bottom = 0;
            it.auto_fit = true;
            it.fit_to_canvas(c.width, c.height);
        });
        self.save_scenes();
    }

    pub fn add_scene_item(&mut self, src: Arc<dyn VideoSource>, created: bool) {
        if created {
            self.engine.add_source(src.clone());
        }
        let c = self.engine.canvas();
        let scene = self.current();
        let id = scene.add(src);
        scene.update(id, |it| it.fit = fit_for(c.width, c.height));
        self.select(Some(id));
        self.save_scenes();
    }

    fn can_change_collection(&mut self) -> bool {
        if self.is_recording() {
            self.message("Scene Collections", "Stop the recording before changing scene collections.");
            return false;
        }
        true
    }

    /// Saves the current collection, removes its sources and opens another (or a new empty one).
    fn switch_collection(&mut self, name: &str, create: bool) {
        if name == self.coll.name && !create {
            return;
        }
        if !self.can_change_collection() {
            return;
        }
        self.save_scenes();
        self.coll.unload(&self.engine.shared);
        let mut warnings = Vec::new();
        let loaded = if create {
            false
        } else {
            self.coll.load(&self.engine.shared, name, &mut |w| warnings.push(w)).unwrap_or_else(|e| {
                warnings.push(format!("\"{name}\" could not be loaded: {e}"));
                false
            })
        };
        if !loaded {
            self.coll.name = name.into();
            self.coll.transition = TransitionKind::Fade;
            self.coll.transition_ms = 300;
            let scene = Scene::new("Scene");
            self.coll.scenes = vec![scene.clone()];
            self.engine.set_current_scene(scene);
        }
        self.s.collection = self.coll.name.clone();
        self.s.save();
        self.save_scenes();
        self.select(None);
        self.transition_ms_text = self.coll.transition_ms.to_string();
        log(format!("collection: {}", self.coll.name));
        if !warnings.is_empty() {
            self.message("Scene Collections", warnings.join("\n"));
        }
    }

    fn add_audio(&mut self, make: impl FnOnce(ebs_engine::audio::AudioClock) -> Arc<AudioSource>) {
        let src = make(self.engine.audio.clock);
        self.engine.audio.add(src);
        self.save_scenes();
    }

    // ------------------------------------------------------------------ recording

    fn on_record(&mut self) {
        match &self.rec {
            RecState::Idle => self.start_recording(false),
            RecState::Recording(_) => self.stop_recording(),
            _ => {}
        }
    }

    fn start_recording(&mut self, low_disk_ok: bool) {
        let rs = self.s.to_recording_settings();
        if let Err(e) = std::fs::create_dir_all(&rs.output_dir) {
            self.message("Recording", format!("Cannot use the recording folder:\n{e}"));
            return;
        }
        if !low_disk_ok {
            if let Some(free) = free_bytes(&rs.output_dir) {
                if free < 2 << 30 {
                    self.confirm(
                        "Low disk space",
                        format!("Only {} MB free on the recording drive. A 1080p60 recording uses roughly 150-300 MB per minute. Record anyway?", free >> 20),
                        "Record",
                        ConfirmAction::RecordLowDisk,
                    );
                    return;
                }
            }
        }
        let engine = self.engine.shared.clone();
        let (tx, rx) = channel();
        std::thread::spawn(move || {
            let _ = tx.send(Recorder::start(&engine, &rs));
        });
        self.rec = RecState::Starting(rx);
    }

    fn stop_recording(&mut self) {
        let RecState::Recording(rec) = &self.rec else { return };
        let rec = rec.clone();
        let (tx, rx) = channel();
        let r2 = rec.clone();
        std::thread::spawn(move || {
            let _ = tx.send(r2.stop());
        });
        self.rec = RecState::Stopping(rec, rx);
    }

    fn poll_recording(&mut self, ctx: &egui::Context) {
        match &self.rec {
            RecState::Starting(rx) => match rx.try_recv() {
                Ok(Ok(rec)) => {
                    let v = &rec.settings.video;
                    log(format!("recording started: {} ({} {}x{}, gpu input {})", rec.path.display(), v.encoder_id, v.width, v.height, rec.gpu_input));
                    self.rec_info = format!(
                        "{}{}\n{}",
                        rec.encoder_name,
                        if rec.gpu_input { " (GPU texture input)" } else { "" },
                        rec.path.file_name().unwrap_or_default().to_string_lossy()
                    );
                    self.stats.last_bytes = 0;
                    self.rec = RecState::Recording(rec);
                }
                Ok(Err(e)) => {
                    log(format!("start failed: {e:#}"));
                    self.rec = RecState::Idle;
                    self.message("Recording", format!("Recording could not start:\n{e:#}"));
                }
                Err(_) => {}
            },
            RecState::Stopping(rec, rx) => {
                if let Ok(r) = rx.try_recv() {
                    let rec = rec.clone();
                    self.rec = RecState::Idle;
                    self.finish_recording(&rec, r);
                    if self.close_after_stop {
                        self.allow_close = true;
                        ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                    }
                }
            }
            _ => {}
        }
    }

    fn finish_recording(&mut self, rec: &Arc<Recorder>, r: anyhow::Result<RecordingResult>) {
        let st = &rec.stats;
        let frames = st.frames_encoded.load(Ordering::Relaxed);
        let dropped = st.frames_dropped.load(Ordering::Relaxed);
        let delay = st.audio_delay_ms.load();
        match &r {
            Ok(res) => {
                let file = res.remuxed.clone().unwrap_or_else(|| res.path.clone());
                let size = std::fs::metadata(&file).map(|m| m.len()).unwrap_or(0);
                self.last_file_text = format!("Saved: {} ({:.1} MB)", file.file_name().unwrap_or_default().to_string_lossy(), size as f64 / 1048576.0);
                self.last_file = Some(file);
                self.rec_info = format!(
                    "{frames} frames encoded, {dropped} dropped (encoder), {} render-lag frames since start{}",
                    self.engine.stats.frames_lagged.load(Ordering::Relaxed),
                    if delay > 0.0 { format!("\nA/V sync: audio delayed {delay:.0} ms to match capture latency") } else { String::new() }
                );
                for w in &res.warnings {
                    log(format!("recording warning: {w}"));
                }
                if !res.warnings.is_empty() {
                    self.message("Recording", res.warnings.join("\n"));
                }
            }
            Err(e) => {
                self.last_file = Some(rec.path.clone());
                self.last_file_text = format!("Saved (with a problem): {}", rec.path.file_name().unwrap_or_default().to_string_lossy());
                self.message("Recording finished with a problem", format!("{e:#}"));
            }
        }
        log(format!("recording stopped: {} {frames} frames, {dropped} dropped, error={}", rec.path.display(), r.as_ref().err().map(|e| e.to_string()).unwrap_or("none".into())));
        // History, one JSON line per recording.
        let line = serde_json::json!({
            "file": self.last_file, "seconds": rec.recorded_seconds(), "bytes": r.as_ref().map(|x| x.bytes).unwrap_or(0),
            "encoder": rec.settings.video.encoder_id, "frames": frames, "dropped": dropped,
            "error": r.as_ref().err().map(|e| e.to_string()),
        });
        use std::io::Write;
        if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(settings::data_dir().join("history.jsonl")) {
            let _ = writeln!(f, "{line}");
        }
    }

    // ------------------------------------------------------------------ settings

    pub fn apply_canvas_from_settings(&mut self) {
        let c = self.engine.canvas();
        let s = &self.s;
        if c.width == s.canvas_width && c.height == s.canvas_height && c.fps_num == s.fps_num && c.fps_den == s.fps_den {
            return;
        }
        let shape_changed = (c.width > c.height) != (s.canvas_width > s.canvas_height) || (c.width == c.height) != (s.canvas_width == s.canvas_height);
        if let Err(e) = self.engine.set_canvas(VideoCanvas::new(s.canvas_width, s.canvas_height, s.fps_num, s.fps_den)) {
            self.message("Video", format!("{e}"));
            return;
        }
        if shape_changed {
            let fit = fit_for(self.s.canvas_width, self.s.canvas_height);
            for sc in &self.coll.scenes {
                for it in sc.snapshot() {
                    if it.auto_fit {
                        sc.update(it.id, |i| i.fit = fit);
                    }
                }
            }
            self.save_scenes();
        }
    }

    pub fn encoder_summary(&self) -> String {
        let v = self.s.effective_video();
        let info = encoding::get(&v.encoder_id);
        let rc = match v.rate_control {
            RateControl::ConstantQuality => format!("CQ {}", v.quality),
            RateControl::Cqp => format!("CQP {}", v.quality),
            RateControl::Vbr => format!("VBR {} Mbps", v.bitrate_kbps / 1000),
            RateControl::Cbr => format!("CBR {} Mbps", v.bitrate_kbps / 1000),
        };
        let preset = match self.s.preset() {
            Some(p) => format!(
                "{}{}",
                p.label(),
                match self.s.result_for(p, None) {
                    None => " (not tested)",
                    Some(r) if r.passed => " (tested)",
                    Some(_) => " (not smooth)",
                }
            ),
            None => "Custom".into(),
        };
        let short = info.display_name.replace("NVIDIA ", "").replace(" (H.265)", "").replace("AMD ", "");
        format!("{preset} · {short} · {rc} · {}x{} {}fps", self.s.output_width, self.s.output_height, fmt_fps(self.s.fps_num, self.s.fps_den))
    }

    fn open_settings(&mut self, page: Option<&str>) {
        if self.is_recording() {
            self.message("Settings", "Stop the recording before changing settings.");
            return;
        }
        self.dialog = Some(Dialog::Settings(Box::new(SettingsState::new(&self.s, page))));
    }

    pub fn open_benchmark(&mut self, encoder_id: &str) {
        if self.is_recording() {
            self.message("Optimise", "Stop the recording before testing presets.");
            return;
        }
        if !self.probe_ready {
            self.message("Optimise", "The encoder check is still running. Try again in a moment.");
            return;
        }
        self.dialog = Some(Dialog::Benchmark(Box::new(BenchState::new(&self.s, encoder_id))));
    }

    fn screenshot(&mut self) {
        let _ = std::fs::create_dir_all(&self.s.output_dir);
        let name = ebs_engine::recorder::format_filename("Screenshot yyyy-MM-dd HH-mm-ss");
        let path = self.s.output_dir.join(format!("{name}.bmp"));
        match self.engine.save_canvas_snapshot(&path) {
            Ok(()) => {
                self.last_file_text = format!("Saved: {}", path.file_name().unwrap_or_default().to_string_lossy());
                self.last_file = Some(path);
            }
            Err(e) => self.message("Screenshot", format!("{e}")),
        }
    }

    // ------------------------------------------------------------------ dialog results

    fn prompt_done(&mut self, action: PromptAction, text: String) {
        let t = text.trim().to_string();
        if t.is_empty() {
            return;
        }
        match action {
            PromptAction::AddScene => {
                let scene = Scene::new(&self.unique_scene_name(&t));
                self.coll.scenes.push(scene.clone());
                self.engine.set_current_scene(scene);
                self.select(None);
                self.save_scenes();
            }
            PromptAction::RenameScene(id) => {
                if let Some(s) = self.scene_by_id(id) {
                    if s.name() != t {
                        let name = if self.coll.scenes.iter().any(|x| x.id != id && x.name() == t) { self.unique_scene_name(&t) } else { t };
                        s.set_name(&name);
                        self.save_scenes();
                    }
                }
            }
            PromptAction::RenameSource(item) => {
                if let Some(it) = self.current().item(item) {
                    *it.source.base().name.lock().unwrap() = t;
                    self.save_scenes();
                }
            }
            PromptAction::RenameAudio(id) => {
                if let Some(a) = self.engine.audio.sources().into_iter().find(|a| a.id == id) {
                    *a.name.lock().unwrap() = t;
                    self.save_scenes();
                }
            }
            PromptAction::NewCollection => {
                let n = self.unique_collection_name(&safe_name(&t));
                self.switch_collection(&n, true);
            }
            PromptAction::DuplicateCollection => {
                if !self.can_change_collection() {
                    return;
                }
                let n = self.unique_collection_name(&safe_name(&t));
                self.save_scenes();
                let from = SceneCollection::path_for(&self.coll.dir, &self.coll.name);
                if let Err(e) = std::fs::copy(&from, SceneCollection::path_for(&self.coll.dir, &n)) {
                    self.message("Duplicate Collection", format!("{e}"));
                    return;
                }
                self.switch_collection(&n, false);
            }
            PromptAction::RenameCollection => {
                let n = safe_name(&t);
                if n == self.coll.name {
                    return;
                }
                if SceneCollection::path_for(&self.coll.dir, &n).exists() {
                    self.message("Rename Collection", format!("A collection called \"{n}\" already exists."));
                    return;
                }
                let old = SceneCollection::path_for(&self.coll.dir, &self.coll.name);
                self.coll.name = n.clone();
                self.save_scenes();
                let _ = std::fs::remove_file(old);
                self.s.collection = n;
                self.s.save();
            }
        }
    }

    fn confirm_done(&mut self, action: ConfirmAction, ctx: &egui::Context) {
        match action {
            ConfirmAction::RemoveScene(id) => self.remove_scene(id),
            ConfirmAction::RemoveSource(item) => self.remove_item(item),
            ConfirmAction::RemoveAudio(id) => {
                self.engine.audio.remove(id);
                self.save_scenes();
            }
            ConfirmAction::DeleteCollection => {
                let others: Vec<String> = SceneCollection::list(&self.coll.dir).into_iter().filter(|c| *c != self.coll.name).collect();
                let Some(next) = others.first().cloned() else { return };
                let victim = SceneCollection::path_for(&self.coll.dir, &self.coll.name);
                self.switch_collection(&next, false);
                if victim.exists() {
                    if let Err(e) = dialogs::recycle(&victim) {
                        self.message("Delete Collection", format!("Could not move it to the Recycle Bin: {e}"));
                    }
                }
            }
            ConfirmAction::RecordLowDisk => self.start_recording(true),
            ConfirmAction::StopAndClose => {
                self.close_after_stop = true;
                self.stop_recording();
                let _ = ctx;
            }
        }
    }

    // ------------------------------------------------------------------ UI

    fn top_bar(&mut self, ui: &mut egui::Ui) {
        ui.horizontal_centered(|ui| {
            ui.label(RichText::new("BETTERBS").font(egui::FontId::new(14.0, egui::FontFamily::Name("bold".into()))).color(TEXT));
            w::vsep(ui);
            let r = egui::menu::menu_custom_button(ui, egui::Button::new(w::label_chevron(&format!("Collection: {}", self.coll.name))), |ui| self.collection_menu(ui));
            if r.inner.is_some() {
                self.popup_open = true;
            }
            r.response.on_hover_text("Scene collections: each has its own scenes and sources. Switch, create, duplicate, rename or delete.");
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.spacing_mut().item_spacing.x = 8.0;
                let recording = self.is_recording();
                if ui.add_enabled(!recording, egui::Button::new(w::icon_label(icon::SETTINGS, "Settings", TEXT))).on_hover_text("Output, encoder, video and audio settings").clicked() {
                    self.open_settings(None);
                }
                let tested = QualityPreset::ALL.iter().all(|p| self.s.result_for(*p, None).is_some());
                let label = if tested { "Optimise" } else { "Optimise (not tested)" };
                let tip = if tested {
                    "Re-test SMOOTH / BALANCED / QUALITY on this PC (about a minute)."
                } else {
                    "The encoder presets have not been tested at this resolution and frame rate yet. Click to record a short test clip with each and pick the best smooth one."
                };
                if ui.button(w::icon_label(icon::OPTIMISE, label, TEXT)).on_hover_text(tip).clicked() {
                    let id = self.s.video.encoder_id.clone();
                    self.open_benchmark(&id);
                }
                if ui.button(w::icon_label(icon::FOLDER, "Recordings", TEXT)).on_hover_text("Open the folder recordings are saved to").clicked() {
                    let _ = std::fs::create_dir_all(&self.s.output_dir);
                    let _ = std::process::Command::new("explorer.exe").arg(&self.s.output_dir).spawn();
                }
                if ui.button(w::icon_label(icon::CAMERA, "Screenshot", TEXT)).on_hover_text("Save the current canvas as an image in the recordings folder").clicked() {
                    self.screenshot();
                }
                ui.add_space(6.0);
                let mut on = self.s.preview_enabled;
                if w::checkbox(ui, &mut on, "Preview").on_hover_text("Turning the preview off saves a little GPU time while gaming. Recording is not affected.").changed() {
                    self.s.preview_enabled = on;
                    self.engine.preview_enabled.store(on, Ordering::Relaxed);
                    self.s.save();
                }
                let mode = self.s.video_mode();
                let mut pick = mode;
                ui.add_space(6.0);
                let cr = w::field_style(ui, |ui| {
                    egui::ComboBox::from_id_salt("mode").width(185.0).height(320.0).icon(w::combo_chevron).selected_text(mode.label()).show_ui(ui, |ui| {
                        for m in VideoMode::ALL {
                            ui.selectable_value(&mut pick, m, m.label());
                        }
                    })
                });
                cr.response.on_hover_text("YouTube Wide video = 1920x1080 landscape. YouTube Short = 1080x1920 vertical (the game fills the frame, its sides are cut off). Custom opens the resolution settings.");
                if pick != mode {
                    if self.is_recording() {
                        self.message("Video mode", "Stop the recording before switching the video mode.");
                    } else if pick == VideoMode::Custom {
                        self.open_settings(Some("Video"));
                    } else {
                        self.s.apply_mode(pick);
                        self.s.save();
                        self.apply_canvas_from_settings();
                    }
                }
                let summary = match &self.probe_status {
                    Some(s) => s.clone(),
                    None => self.encoder_summary(),
                };
                ui.add_space(4.0);
                ui.label(RichText::new(summary).color(MUTED).size(11.0));
            });
        });
    }

    fn collection_menu(&mut self, ui: &mut egui::Ui) {
        let mut names = SceneCollection::list(&self.coll.dir);
        if !names.contains(&self.coll.name) {
            names.push(self.coll.name.clone());
            names.sort_by_key(|n| n.to_lowercase());
        }
        for n in names {
            if ui.radio(n == self.coll.name, &n).clicked() {
                ui.close_menu();
                self.switch_collection(&n, false);
            }
        }
        ui.separator();
        if ui.button("New collection...").clicked() {
            ui.close_menu();
            let n = self.unique_collection_name("Collection");
            self.prompt("New Collection", "Collection name", &n, PromptAction::NewCollection);
        }
        if ui.button("Duplicate collection...").clicked() {
            ui.close_menu();
            let n = self.unique_collection_name(&self.coll.name.clone());
            self.prompt("Duplicate Collection", "Name of the copy", &n, PromptAction::DuplicateCollection);
        }
        if ui.button("Rename collection...").clicked() {
            ui.close_menu();
            let n = self.coll.name.clone();
            self.prompt("Rename Collection", "Collection name", &n, PromptAction::RenameCollection);
        }
        if ui.button("Delete collection...").clicked() {
            ui.close_menu();
            let others = SceneCollection::list(&self.coll.dir).into_iter().filter(|c| *c != self.coll.name).count();
            if others == 0 {
                self.message("Delete Collection", "This is the only collection. Create another one before deleting it.");
            } else if self.can_change_collection() {
                let msg = format!("Delete the collection \"{}\" and its scenes? It goes to the Recycle Bin, so it can be restored from there.", self.coll.name);
                self.confirm("Delete Collection", msg, "Delete", ConfirmAction::DeleteCollection);
            }
        }
    }

    fn preview_panel(&mut self, ui: &mut egui::Ui, modal: bool) {
        let rect = ui.available_rect_before_wrap();
        let resp = ui.allocate_rect(rect, Sense::click_and_drag());
        ui.painter().rect_filled(rect, 4.0, Color32::from_rgb(0x0F, 0x10, 0x12));
        ui.painter().rect_stroke(rect.shrink(0.5), 4.0, egui::Stroke::new(1.0, LINE), egui::StrokeKind::Middle);
        if !self.s.preview_enabled {
            ui.painter().text(rect.center(), egui::Align2::CENTER_CENTER, "Preview off", egui::FontId::proportional(14.0), MUTED);
        }
        let ppp = ui.ctx().pixels_per_point();
        let inner = rect.shrink(1.0);
        let visible = self.s.preview_enabled && !self.popup_open && inner.width() > 4.0 && inner.height() > 4.0;
        if let Some(p) = &mut self.preview {
            let (x, y) = ((inner.min.x * ppp).round() as i32, (inner.min.y * ppp).round() as i32);
            let (w, h) = ((inner.width() * ppp).round() as i32, (inner.height() * ppp).round() as i32);
            if p.place(x, y, w, h, visible) {
                self.engine.resize_preview(w.max(1) as u32, h.max(1) as u32);
            }
        }
        if modal || !self.s.preview_enabled {
            return;
        }
        let scene = self.current();
        let canvas = self.engine.canvas();
        let events = self.editor.ui(ui, &resp, inner, canvas, &scene, self.selected);
        for e in events {
            match e {
                PreviewEvent::Selected(id) => self.select(id),
                PreviewEvent::Changed => self.save_scenes(),
                PreviewEvent::Context(_) => {}
            }
        }
        let sel = self.selected;
        let menu = resp.context_menu(|ui| {
            if let Some(id) = sel {
                self.item_menu(ui, id);
            } else if ui.button("Add source...").clicked() {
                ui.close_menu();
                self.dialog = Some(Dialog::AddSource(Box::new(AddSourceState::new(self))));
            }
        });
        if menu.is_some() {
            self.popup_open = true;
        }
    }

    fn item_menu(&mut self, ui: &mut egui::Ui, id: u64) {
        let scene = self.current();
        let Some(it) = scene.item(id) else { return };
        if ui.button("Transform...").clicked() {
            ui.close_menu();
            self.dialog = Some(Dialog::Transform { item: id, fields: dialogs::TransformFields::load(&it) });
        }
        if ui.button("Fit to screen").clicked() {
            ui.close_menu();
            self.fit_item(id);
        }
        if ui.button(if it.visible { "Hide" } else { "Show" }).clicked() {
            ui.close_menu();
            scene.update(id, |i| i.visible = !i.visible);
            self.save_scenes();
        }
        if ui.button(if it.locked { "Unlock" } else { "Lock" }).clicked() {
            ui.close_menu();
            scene.update(id, |i| i.locked = !i.locked);
            self.save_scenes();
        }
        if ui.button("Rename...").clicked() {
            ui.close_menu();
            self.prompt("Rename Source", "Source name", &it.source.name(), PromptAction::RenameSource(id));
        }
        ui.separator();
        if ui.button("Remove").clicked() {
            ui.close_menu();
            self.confirm("Remove Source", format!("Remove \"{}\" from this scene?", it.source.name()), "Remove", ConfirmAction::RemoveSource(id));
        }
    }

    fn header(ui: &mut egui::Ui, t: &str) {
        w::header(ui, t);
    }

    fn icon_button(ui: &mut egui::Ui, glyph: &str, tip: &str) -> bool {
        // Screen readers / UI automation get the tooltip as the button's name instead of the icon glyph.
        w::icon_button(ui, glyph, tip, true).clicked()
    }

    fn scenes_panel(&mut self, ui: &mut egui::Ui) {
        Self::header(ui, "SCENES");
        let cur = self.current();
        egui::TopBottomPanel::bottom("scene_tools").frame(egui::Frame::NONE).show_separator_line(false).show_inside(ui, |ui| {
            ui.add_space(2.0);
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 4.0;
                ui.add_space(2.0);
                let mut kind = self.coll.transition;
                let cw = ui.available_width() - 62.0;
                let cr = w::field_style(ui, |ui| {
                    egui::ComboBox::from_id_salt("transition").width(cw).icon(w::combo_chevron).selected_text(kind.label()).show_ui(ui, |ui| {
                        for k in TransitionKind::ALL {
                            ui.selectable_value(&mut kind, k, k.label());
                        }
                    })
                });
                cr.response.on_hover_text("How the picture changes when you click another scene. Recordings include the transition.");
                if kind != self.coll.transition {
                    self.coll.transition = kind;
                    self.save_scenes();
                }
                let te = ui.add_enabled(kind != TransitionKind::Cut, egui::TextEdit::singleline(&mut self.transition_ms_text).desired_width(46.0).min_size(Vec2::new(54.0, 26.0)).vertical_align(egui::Align::Center).background_color(FIELD));
                te.clone().on_hover_text("Transition length in milliseconds (300 is a quick, smooth fade).");
                if te.lost_focus() {
                    let ms = self.transition_ms_text.trim().parse::<u32>().map(|v| v.clamp(50, 5000)).unwrap_or(300);
                    self.coll.transition_ms = ms;
                    self.transition_ms_text = ms.to_string();
                    self.save_scenes();
                }
            });
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 0.0;
                if Self::icon_button(ui, icon::ADD, "Add scene") {
                    let n = self.unique_scene_name("Scene");
                    self.prompt("Add Scene", "Scene name", &n, PromptAction::AddScene);
                }
                if Self::icon_button(ui, icon::REMOVE, "Remove scene") {
                    if self.coll.scenes.len() <= 1 {
                        self.message("Remove Scene", "A collection needs at least one scene.");
                    } else {
                        self.confirm("Remove Scene", format!("Remove scene \"{}\"?", cur.name()), "Remove", ConfirmAction::RemoveScene(cur.id));
                    }
                }
                if Self::icon_button(ui, icon::RENAME, "Rename scene") {
                    self.prompt("Rename Scene", "Scene name", &cur.name(), PromptAction::RenameScene(cur.id));
                }
                if Self::icon_button(ui, icon::COPY, "Duplicate scene (same sources, own layout)") {
                    self.duplicate_scene();
                }
                if Self::icon_button(ui, icon::UP, "Move up") {
                    self.move_scene(-1);
                }
                if Self::icon_button(ui, icon::DOWN, "Move down") {
                    self.move_scene(1);
                }
            });
        });
        egui::ScrollArea::vertical().auto_shrink([false; 2]).show(ui, |ui| {
            let scenes = self.coll.scenes.clone();
            ui.spacing_mut().item_spacing.y = 0.0;
            for s in scenes {
                let sel = Arc::ptr_eq(&s, &cur);
                let (r, _) = w::list_row(ui, egui::Id::new(("scene_row", s.id)), sel, |ui| ui.label(s.name()));
                r.widget_info(|| egui::WidgetInfo::selected(egui::WidgetType::SelectableLabel, true, sel, s.name()));
                if r.clicked() {
                    self.switch_scene(s.clone());
                }
                if r.double_clicked() {
                    self.prompt("Rename Scene", "Scene name", &s.name(), PromptAction::RenameScene(s.id));
                }
            }
        });
    }

    fn sources_panel(&mut self, ui: &mut egui::Ui) {
        Self::header(ui, "SOURCES");
        let scene = self.current();
        egui::TopBottomPanel::bottom("source_tools").frame(egui::Frame::NONE).show_separator_line(false).show_inside(ui, |ui| {
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 0.0;
                if Self::icon_button(ui, icon::ADD, "Add a source to this scene") {
                    self.dialog = Some(Dialog::AddSource(Box::new(AddSourceState::new(self))));
                }
                let sel = self.selected.and_then(|id| scene.item(id));
                if Self::icon_button(ui, icon::REMOVE, "Remove from this scene") {
                    if let Some(it) = &sel {
                        self.confirm("Remove Source", format!("Remove \"{}\" from this scene?", it.source.name()), "Remove", ConfirmAction::RemoveSource(it.id));
                    }
                }
                // The list is drawn top-first (top of the list = drawn on top), so "up" is later in draw order.
                if Self::icon_button(ui, icon::UP, "Move up (draw on top)") {
                    if let Some(it) = &sel {
                        scene.move_by(it.id, 1);
                        self.save_scenes();
                    }
                }
                if Self::icon_button(ui, icon::DOWN, "Move down (draw below)") {
                    if let Some(it) = &sel {
                        scene.move_by(it.id, -1);
                        self.save_scenes();
                    }
                }
                if Self::icon_button(ui, icon::FIT, "Fit to screen (keeps aspect ratio)") {
                    if let Some(it) = &sel {
                        self.fit_item(it.id);
                    }
                }
                if Self::icon_button(ui, icon::TRANSFORM, "Transform: position, size, rotation, crop, opacity") {
                    if let Some(it) = &sel {
                        self.dialog = Some(Dialog::Transform { item: it.id, fields: dialogs::TransformFields::load(it) });
                    }
                }
            });
        });
        egui::ScrollArea::vertical().auto_shrink([false; 2]).show(ui, |ui| {
            let items: Vec<_> = scene.snapshot().into_iter().rev().collect();
            if items.is_empty() {
                ui.label(RichText::new("No sources. Use + to add game, window or display capture.").color(MUTED));
            }
            ui.spacing_mut().item_spacing.y = 0.0;
            for it in items {
                let selected = self.selected == Some(it.id);
                let (r, _) = w::list_row(ui, egui::Id::new(("src_row", it.id)), selected, |ui| {
                        ui.horizontal(|ui| {
                            let kind = match it.source.kind() {
                                "Game Capture" => icon::GAME,
                                "Window Capture" => icon::WINDOW,
                                "Display Capture" => icon::DISPLAY,
                                "Scene" => icon::SCENE,
                                _ => icon::PATTERN,
                            };
                            ui.label(icon_text(kind).color(MUTED));
                            ui.add_space(2.0);
                            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                ui.spacing_mut().item_spacing.x = 0.0;
                                if w::toggle_icon(ui, if it.locked { icon::LOCK } else { icon::UNLOCK }, it.locked, None, "Lock (prevents moving it in the preview)").clicked() {
                                    scene.update(it.id, |i| i.locked = !i.locked);
                                    self.save_scenes();
                                }
                                if w::toggle_icon(ui, if it.visible { icon::EYE } else { icon::EYE_OFF }, it.visible, None, "Show / hide").clicked() {
                                    scene.update(it.id, |i| i.visible = !i.visible);
                                    self.save_scenes();
                                }
                                ui.with_layout(egui::Layout::top_down(egui::Align::Min), |ui| {
                                    ui.spacing_mut().item_spacing.y = 0.0;
                                    ui.add(egui::Label::new(it.source.name()).truncate());
                                    ui.add(egui::Label::new(RichText::new({ let (sw, sh) = it.source.size(); if sw > 0 { format!("{} · {}x{} · {}", it.source.kind(), sw, sh, it.source.status()) } else { format!("{} · {}", it.source.kind(), it.source.status()) } }).color(MUTED).size(10.0)).truncate());
                                });
                            });
                        });
                    });
                if r.clicked() {
                    self.select(Some(it.id));
                }
                if r.double_clicked() {
                    self.dialog = Some(Dialog::Transform { item: it.id, fields: dialogs::TransformFields::load(&it) });
                }
                let id = it.id;
                if r.context_menu(|ui| self.item_menu(ui, id)).is_some() {
                    self.popup_open = true;
                }
            }
        });
    }

    fn mixer_panel(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.vertical(|ui| Self::header(ui, "AUDIO MIXER"));
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let r = egui::menu::menu_custom_button(ui, egui::Button::new(icon_text(icon::ADD).size(12.0)).frame(false).min_size(Vec2::new(26.0, 24.0)), |ui| {
                    ui.menu_button("Desktop audio (what you hear)", |ui| {
                        for d in list_devices(false).unwrap_or_default() {
                            if ui.button(format!("{}{}", d.name, if d.is_default { "  (default)" } else { "" })).clicked() {
                                ui.close_menu();
                                let (id, name) = (d.id.clone(), format!("Desktop: {}", shorten(&d.name, 28)));
                                self.add_audio(move |c| AudioSource::desktop(c, Some(id), &name));
                            }
                        }
                    });
                    ui.menu_button("Microphone / input", |ui| {
                        for d in list_devices(true).unwrap_or_default() {
                            if ui.button(format!("{}{}", d.name, if d.is_default { "  (default)" } else { "" })).clicked() {
                                ui.close_menu();
                                let (id, name) = (d.id.clone(), format!("Mic: {}", shorten(&d.name, 28)));
                                self.add_audio(move |c| AudioSource::microphone(c, Some(id), &name));
                            }
                        }
                    });
                });
                if r.inner.is_some() {
                    self.popup_open = true;
                }
                r.response.on_hover_text("Add an audio device");
            });
        });
        egui::ScrollArea::vertical().auto_shrink([false; 2]).show(ui, |ui| {
            ui.spacing_mut().item_spacing.y = 0.0;
            let sources = self.engine.audio.sources();
            if sources.is_empty() {
                ui.label(RichText::new("No audio sources. Use + to add desktop audio or a microphone.").color(MUTED));
            }
            for a in sources {
                ui.add_space(2.0);
                egui::Frame::NONE.inner_margin(egui::Margin { left: 4, right: 4, top: 0, bottom: 0 }).show(ui, |ui| self.mixer_strip(ui, &a));
                ui.add_space(8.0);
            }
        });
    }

    fn mixer_strip(&mut self, ui: &mut egui::Ui, a: &Arc<AudioSource>) {
        let hold = self.meters.entry(a.id).or_insert((0.0, 0.0));
        // Peak hold with ~20 dB/s fall so short transients stay visible.
        hold.0 = a.peak_l.load().max(hold.0 * 0.92);
        hold.1 = a.peak_r.load().max(hold.1 * 0.92);
        let (hl, hr) = *hold;
        let muted = a.muted.load(Ordering::Relaxed);
        let resp = ui
            .vertical(|ui| {
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = 4.0;
                    ui.add(egui::Label::new(a.name()).truncate()).on_hover_text(format!("{}\n{}\nRight-click for tracks and options", a.name(), a.format.lock().unwrap()));
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if w::toggle_icon(ui, if muted { icon::MUTE } else { icon::VOLUME }, muted, Some(if muted { REC } else { MUTED }), "Mute").clicked() {
                            a.muted.store(!muted, Ordering::Relaxed);
                            self.save_scenes();
                        }
                        let peak = hl.max(hr);
                        let db = if peak <= 1e-5 || muted { "-inf dB".to_string() } else { format!("{:.1} dB", 20.0 * peak.log10()) };
                        ui.label(RichText::new(db).monospace().size(10.0).color(if peak > 0.89 { Color32::from_rgb(0xFF, 0x45, 0x00) } else { Color32::from_rgb(0x80, 0x80, 0x80) }));
                    });
                });
                let frac = |pk: f32| if pk <= 1e-5 { 0.0 } else { ((20.0 * pk.log10() + 60.0) / 60.0).clamp(0.0, 1.0) };
                w::meter(ui, frac(hl), frac(hr));
                // Fader in dB: -60 (silent) .. +12.
                let vol = a.volume.load();
                let mut db = if vol <= 0.001 { -60.0 } else { (20.0 * vol.log10()).clamp(-60.0, 12.0) };
                let s = w::fader(ui, egui::Id::new(("fader", a.id)), &mut db, -60.0..=12.0, 0.0);
                let s = s.on_hover_text(format!("Volume {db:+.1} dB. Double-click to reset to 0 dB."));
                s.widget_info(|| egui::WidgetInfo::slider(true, db as f64, format!("{} volume", a.name())));
                if s.changed() || s.double_clicked() {
                    a.volume.store(if db <= -59.9 { 0.0 } else { 10f32.powf(db / 20.0) });
                }
                if s.drag_stopped() || s.double_clicked() || (s.changed() && !s.dragged()) {
                    self.save_scenes();
                }
            })
            .response;
        let r = ui.interact(resp.rect, egui::Id::new(("strip", a.id)), Sense::click());
        let a2 = a.clone();
        if r.context_menu(|ui| {
            ui.menu_button("Recording tracks", |ui| {
                let names = ["1 (Mix)", "2 (Desktop)", "3 (Mic)", "4", "5", "6"];
                for (t, n) in names.iter().enumerate() {
                    let bit = 1u32 << t;
                    let mut on = a2.track_mask.load(Ordering::Relaxed) & bit != 0;
                    if ui.checkbox(&mut on, format!("Track {n}")).changed() {
                        let m = a2.track_mask.load(Ordering::Relaxed);
                        a2.track_mask.store(if on { m | bit } else { m & !bit }, Ordering::Relaxed);
                        self.save_scenes();
                    }
                }
            });
            if ui.button("Rename...").clicked() {
                ui.close_menu();
                self.prompt("Rename Audio", "Name", &a2.name(), PromptAction::RenameAudio(a2.id));
            }
            if ui.button("Remove").clicked() {
                ui.close_menu();
                self.confirm("Remove Audio", format!("Remove \"{}\"?", a2.name()), "Remove", ConfirmAction::RemoveAudio(a2.id));
            }
        })
        .is_some()
        {
            self.popup_open = true;
        }
    }

    fn controls_panel(&mut self, ui: &mut egui::Ui) {
        Self::header(ui, "CONTROLS");
        let (label, recording, enabled) = match &self.rec {
            RecState::Idle => ("Start Recording", false, self.probe_ready),
            RecState::Starting(_) => ("Starting...", false, false),
            RecState::Recording(_) => ("Stop Recording", true, true),
            RecState::Stopping(..) => ("Stopping...", true, false),
        };
        let r = w::rec_button(ui, label, recording, enabled);
        if r.clicked() {
            log(format!("record button: {}", if self.is_recording() { "stop" } else { "start" }));
            self.on_record();
        }
        let rec = match &self.rec {
            RecState::Recording(r) => Some(r.clone()),
            _ => None,
        };
        let paused = rec.as_ref().is_some_and(|r| r.is_paused());
        ui.add_space(2.0);
        let pb = egui::Button::new(w::icon_label(if paused { icon::PLAY } else { icon::PAUSE }, if paused { "Resume" } else { "Pause" }, TEXT)).min_size(Vec2::new(ui.available_width(), 26.0));
        if ui.add_enabled(rec.is_some(), pb).clicked() {
            if let Some(r) = &rec {
                r.set_paused(!paused);
            }
        }
        ui.add_space(12.0);
        let (dur, size) = match &self.rec {
            RecState::Recording(r) | RecState::Stopping(r, _) => {
                let b = r.bytes_written();
                (hms(r.recorded_seconds()), if b >= 1 << 30 { format!("{:.2} GB", b as f64 / 1073741824.0) } else { format!("{:.0} MB", b as f64 / 1048576.0) })
            }
            _ => ("00:00:00".into(), "-".into()),
        };
        ui.columns(2, |c| {
            for (col, title, value) in [(0, "DURATION", dur), (1, "FILE SIZE", size)] {
                c[col].spacing_mut().item_spacing.y = 0.0;
                c[col].label(RichText::new(title).color(MUTED).size(10.0));
                c[col].label(RichText::new(value).monospace().size(18.0).color(TEXT));
            }
        });
        ui.add_space(6.0);
        ui.label(RichText::new(&self.rec_info).color(MUTED).size(11.0));
        if !self.last_file_text.is_empty() {
            let r = ui.add(egui::Label::new(RichText::new(&self.last_file_text).color(MUTED).size(11.0)).sense(Sense::click())).on_hover_text("Show in Explorer");
            if r.clicked() {
                if let Some(f) = self.last_file.as_ref().filter(|f| f.exists()) {
                    let _ = std::process::Command::new("explorer.exe").arg(format!("/select,{}", f.display())).spawn();
                }
            }
        }
    }

    fn update_stats(&mut self) {
        if self.last_stats.elapsed() < Duration::from_secs(1) {
            return;
        }
        let secs = self.last_stats.elapsed().as_secs_f64().min(5.0);
        self.last_stats = Instant::now();
        let s = self.monitor.sample();
        // Capture fps of the largest visible source (normally the game).
        let main = self
            .current()
            .snapshot()
            .into_iter()
            .filter(|i| i.visible && i.source.size().0 > 0)
            .max_by(|a, b| (a.display_width() * a.display_height()).total_cmp(&(b.display_width() * b.display_height())))
            .map(|i| i.source);
        let frames = main.as_ref().map(|m| m.frames_delivered()).unwrap_or(0);
        let cap = if main.as_ref().map(|m| m.id()) != self.stats.cap_source {
            self.stats.cap_source = main.as_ref().map(|m| m.id());
            0.0
        } else {
            (frames.saturating_sub(self.stats.cap_frames)) as f64 / secs
        };
        self.stats.cap_frames = frames;
        let gpu = match s.gpu {
            Some(g) => format!("GPU {:>3}%  NVENC {:>3}%  VRAM {}/{} MB", g.gpu_percent, g.encoder_percent, g.vram_used_mb, g.vram_total_mb),
            None => "GPU n/a".into(),
        };
        self.stats.perf = format!(
            "CPU {:>3.0}% (app {:.1}%)  {gpu}  RAM {:.1}/{:.1} GB  capture {cap:.0} fps  render lag {}  render {:.2} ms",
            s.system_cpu_percent,
            s.app_cpu_percent,
            s.ram_used_mb as f64 / 1024.0,
            s.ram_total_mb as f64 / 1024.0,
            self.engine.stats.frames_lagged.load(Ordering::Relaxed),
            self.engine.stats.average_render_ms()
        );
        self.stats.disk = free_bytes(&self.s.output_dir)
            .map(|f| format!("{} {:.1} GB free", self.s.output_dir.components().next().map(|c| c.as_os_str().to_string_lossy().into_owned()).unwrap_or_default(), f as f64 / 1073741824.0))
            .unwrap_or_default();
        if let RecState::Recording(r) = &self.rec {
            let b = r.bytes_written();
            self.stats.mbps = (b - self.stats.last_bytes) as f64 * 8.0 / 1e6 / secs;
            self.stats.last_bytes = b;
        }
    }

    fn status_bar(&mut self, ui: &mut egui::Ui) {
        ui.horizontal_centered(|ui| {
            let (text, color) = match &self.rec {
                RecState::Recording(r) => {
                    let paused = r.is_paused();
                    (
                        format!(
                            "{} {}  {:.1} Mbps  enc drops {}  queue {}",
                            if paused { "PAUSED" } else { "REC" },
                            hms(r.recorded_seconds()),
                            self.stats.mbps,
                            r.stats.frames_dropped.load(Ordering::Relaxed),
                            r.stats.queue.load(Ordering::Relaxed)
                        ),
                        if paused { WARN } else { REC },
                    )
                }
                RecState::Starting(_) => ("STARTING".into(), WARN),
                RecState::Stopping(..) => ("STOPPING".into(), WARN),
                RecState::Idle => ("IDLE".into(), MUTED),
            };
            ui.add(egui::Label::new(RichText::new(text).monospace().size(12.0).color(color)));
            ui.add_space(16.0);
            ui.label(RichText::new(&self.stats.perf).monospace().color(MUTED).size(11.0));
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.label(RichText::new(&self.stats.disk).color(MUTED).size(11.0));
            });
        });
    }
}

impl eframe::App for EbsApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.poll_probe();
        self.poll_recording(ctx);
        self.update_stats();
        for l in self.engine.drain_log() {
            log(format!("engine: {l}"));
        }

        // Closing: never lose a recording.
        if ctx.input(|i| i.viewport().close_requested()) && !self.allow_close {
            if self.is_recording() {
                ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
                if matches!(self.rec, RecState::Recording(_)) && self.dialog.is_none() {
                    self.confirm("Recording in progress", "A recording is running. Stop it and close BETTERBS?", "Stop and close", ConfirmAction::StopAndClose);
                }
            } else if matches!(self.dialog, Some(Dialog::Benchmark(ref b)) if b.running()) {
                ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            }
        }

        let modal = self.dialog.is_some();
        let popup_last = self.popup_open;
        self.popup_open = ctx.memory(|m| m.any_popup_open());

        egui::TopBottomPanel::top("top").exact_height(38.0).frame(egui::Frame::NONE.fill(PANEL).inner_margin(egui::Margin::symmetric(10, 0))).show(ctx, |ui| {
            ui.add_enabled_ui(!modal, |ui| self.top_bar(ui));
        });
        egui::TopBottomPanel::bottom("status").exact_height(26.0).frame(egui::Frame::NONE.fill(PANEL).inner_margin(egui::Margin::symmetric(10, 0))).show(ctx, |ui| {
            self.status_bar(ui);
        });
        let card = egui::Frame::NONE.fill(PANEL).stroke(egui::Stroke::new(1.0, LINE)).corner_radius(4.0).inner_margin(egui::Margin::same(4)).outer_margin(egui::Margin::same(3));
        let controls_card = card.inner_margin(egui::Margin { left: 8, right: 8, top: 4, bottom: 8 });
        egui::TopBottomPanel::bottom("dock").exact_height(252.0).show_separator_line(false).frame(egui::Frame::NONE.fill(BG).inner_margin(egui::Margin { left: 0, right: 0, top: 2, bottom: 0 })).show(ctx, |ui| {
            ui.add_enabled_ui(!modal, |ui| {
                egui::SidePanel::left("scenes").exact_width(190.0).resizable(false).show_separator_line(false).frame(card).show_inside(ui, |ui| self.scenes_panel(ui));
                egui::SidePanel::left("sources").exact_width(290.0).resizable(false).show_separator_line(false).frame(card).show_inside(ui, |ui| self.sources_panel(ui));
                egui::SidePanel::right("controls").exact_width(230.0).resizable(false).show_separator_line(false).frame(controls_card).show_inside(ui, |ui| self.controls_panel(ui));
                egui::CentralPanel::default().frame(card).show_inside(ui, |ui| self.mixer_panel(ui));
            });
        });
        egui::CentralPanel::default().frame(egui::Frame::NONE.fill(BG).inner_margin(egui::Margin { left: 3, right: 3, top: 3, bottom: 0 })).show(ctx, |ui| {
            self.preview_panel(ui, modal);
        });
        // A popup opened this frame: the child preview window would cover it, so hide it now (next frame shows it).
        if self.popup_open && !popup_last {
            ctx.request_repaint();
        }

        if self.dialog.is_some() {
            dialogs::show(self, ctx);
        }
        ctx.request_repaint_after(Duration::from_millis(33));
    }

    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        log("app closing");
        self.save_scenes();
        self.s.save();
        self.engine.detach_preview();
        self.preview = None;
    }
}

pub fn handle_dialog_result(app: &mut EbsApp, ctx: &egui::Context, result: dialogs::DialogResult) {
    match result {
        dialogs::DialogResult::None => {}
        dialogs::DialogResult::Close => app.dialog = None,
        dialogs::DialogResult::PromptOk(action, text) => {
            app.dialog = None;
            app.prompt_done(action, text);
        }
        dialogs::DialogResult::ConfirmOk(action) => {
            app.dialog = None;
            app.confirm_done(action, ctx);
        }
        dialogs::DialogResult::AddSource(src, created) => {
            app.dialog = None;
            app.add_scene_item(src, created);
        }
        dialogs::DialogResult::SettingsSaved { reprobe } => {
            app.dialog = None;
            app.s.save();
            app.engine.preview_divisor.store(app.s.preview_fps_divisor.max(1), Ordering::Relaxed);
            app.apply_canvas_from_settings();
            if reprobe {
                app.start_probe(true);
            }
        }
        dialogs::DialogResult::Error(title, msg) => {
            app.dialog = Some(Dialog::Message { title, message: msg });
        }
    }
}

pub fn fmt_fps(num: u32, den: u32) -> String {
    let f = num as f64 / den.max(1) as f64;
    let s = format!("{f:.2}");
    s.trim_end_matches('0').trim_end_matches('.').to_string()
}

fn hms(secs: f64) -> String {
    let s = secs.max(0.0) as u64;
    format!("{:02}:{:02}:{:02}", s / 3600, (s / 60) % 60, s % 60)
}

fn shorten(s: &str, n: usize) -> String {
    if s.chars().count() > n {
        s.chars().take(n).collect::<String>() + "\u{2026}"
    } else {
        s.to_string()
    }
}

pub fn free_bytes(dir: &std::path::Path) -> Option<u64> {
    let mut p = dir.to_path_buf();
    while !p.exists() {
        p = p.parent()?.to_path_buf();
    }
    let h = windows::core::HSTRING::from(p.as_os_str());
    let mut free = 0u64;
    unsafe { windows::Win32::Storage::FileSystem::GetDiskFreeSpaceExW(&h, Some(&mut free), None, None) }.ok()?;
    Some(free)
}

fn setup_style(ctx: &egui::Context) {
    let mut fonts = egui::FontDefinitions::default();
    let windir = std::env::var("WINDIR").unwrap_or_else(|_| r"C:\Windows".into());
    let fdir = PathBuf::from(windir).join("Fonts");
    if let Ok(b) = std::fs::read(fdir.join("segoeui.ttf")) {
        fonts.font_data.insert("segoe".into(), Arc::new(egui::FontData::from_owned(b)));
        fonts.families.get_mut(&egui::FontFamily::Proportional).unwrap().insert(0, "segoe".into());
    }
    if let Ok(b) = std::fs::read(fdir.join("consola.ttf")) {
        fonts.font_data.insert("consolas".into(), Arc::new(egui::FontData::from_owned(b)));
        fonts.families.get_mut(&egui::FontFamily::Monospace).unwrap().insert(0, "consolas".into());
    }
    for (key, file) in [("semibold", "seguisb.ttf"), ("bold", "segoeuib.ttf")] {
        let fam = egui::FontFamily::Name(key.into());
        let mut list = Vec::new();
        if let Ok(b) = std::fs::read(fdir.join(file)) {
            fonts.font_data.insert(key.into(), Arc::new(egui::FontData::from_owned(b)));
            list.push(key.to_string());
        }
        list.extend(fonts.families[&egui::FontFamily::Proportional].iter().cloned());
        fonts.families.insert(fam, list);
    }
    let icons = ["SegoeIcons.ttf", "segmdl2.ttf"].iter().find_map(|f| std::fs::read(fdir.join(f)).ok());
    if let Some(b) = icons {
        fonts.font_data.insert("mdl2".into(), Arc::new(egui::FontData::from_owned(b)));
        let mut prop = fonts.families[&egui::FontFamily::Proportional].clone();
        // Icons-only family, and a "mixed" family for buttons with an icon and text.
        fonts.families.insert(egui::FontFamily::Name("icons".into()), vec!["mdl2".into()]);
        prop.push("mdl2".into());
        fonts.families.insert(egui::FontFamily::Name("mixed".into()), prop.clone());
        // Plain text also falls back to the icon font, so "{glyph}  Settings" buttons render.
        fonts.families.insert(egui::FontFamily::Proportional, prop);
    } else {
        let prop = fonts.families[&egui::FontFamily::Proportional].clone();
        fonts.families.insert(egui::FontFamily::Name("icons".into()), prop.clone());
        fonts.families.insert(egui::FontFamily::Name("mixed".into()), prop);
    }
    ctx.set_fonts(fonts);

    let mut v = egui::Visuals::dark();
    v.panel_fill = BG;
    v.window_fill = Color32::from_rgb(0x24, 0x27, 0x2C);
    v.window_stroke = egui::Stroke::new(1.0, LINE);
    v.window_corner_radius = egui::CornerRadius::same(4);
    v.menu_corner_radius = egui::CornerRadius::same(3);
    v.popup_shadow = egui::epaint::Shadow { offset: [0, 2], blur: 6, spread: 0, color: Color32::from_black_alpha(90) };
    v.extreme_bg_color = FIELD;
    v.faint_bg_color = PANEL;
    v.button_frame = true;
    // WPF buttons: PanelHi fill, 1 px line, 3 px corners; #30343A on hover, #3A3F46 pressed. No grow on hover.
    let line = egui::Stroke::new(1.0, LINE);
    let ws = &mut v.widgets;
    ws.noninteractive.bg_stroke = line;
    ws.noninteractive.fg_stroke = egui::Stroke::new(1.0, TEXT);
    ws.noninteractive.bg_fill = PANEL;
    for (st, fill) in [(&mut ws.inactive, PANEL_HI), (&mut ws.hovered, w::HOVER), (&mut ws.active, w::PRESS), (&mut ws.open, w::PRESS)] {
        st.bg_fill = fill;
        st.weak_bg_fill = fill;
        st.bg_stroke = line;
        st.fg_stroke = egui::Stroke::new(1.0, TEXT);
        st.corner_radius = egui::CornerRadius::same(3);
        st.expansion = 0.0;
    }
    for st in [&mut ws.noninteractive, &mut ws.inactive, &mut ws.hovered, &mut ws.active, &mut ws.open] {
        st.corner_radius = egui::CornerRadius::same(3);
    }
    v.selection.bg_fill = SELECT;
    v.selection.stroke = egui::Stroke::new(1.0, ACCENT);
    v.slider_trailing_fill = true;
    v.hyperlink_color = ACCENT;
    v.striped = false;
    // Always dark: with Windows in light mode egui would otherwise switch to its light style (white buttons, grey text).
    ctx.set_theme(egui::Theme::Dark);
    ctx.set_visuals_of(egui::Theme::Dark, v.clone());
    ctx.set_visuals_of(egui::Theme::Light, v);
    ctx.all_styles_mut(|s| {
        s.spacing.item_spacing = Vec2::new(6.0, 4.0);
        s.spacing.button_padding = Vec2::new(10.0, 4.0);
        s.spacing.interact_size.y = 26.0;
        s.spacing.menu_margin = egui::Margin::same(2);
        s.spacing.scroll = egui::style::ScrollStyle::thin();
        s.text_styles.insert(egui::TextStyle::Body, egui::FontId::proportional(12.0));
        s.text_styles.insert(egui::TextStyle::Button, egui::FontId::proportional(12.0));
        s.text_styles.insert(egui::TextStyle::Small, egui::FontId::proportional(10.0));
        s.text_styles.insert(egui::TextStyle::Monospace, egui::FontId::monospace(12.0));
    });
}

