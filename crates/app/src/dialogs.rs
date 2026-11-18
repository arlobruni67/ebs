//! Dialogs. Each is a separate native window (egui immediate viewport), so the preview child window of
//! the main window never covers it. One dialog is open at a time; the main window is disabled meanwhile.
use crate::app::{handle_dialog_result, ConfirmAction, Dialog, EbsApp, PromptAction, GOOD, MUTED, WARN};
use crate::settings::{log, AppSettings, VideoMode};
use eframe::egui::{self, RichText, Vec2};
use ebs_engine::benchmark::{self, BenchProgress, PresetBenchmark, PresetTestResult};
use ebs_engine::capture::{enumerate_monitors, enumerate_windows, MonitorTarget, WindowTarget, KNOWN_GAME_EXES};
use ebs_engine::encoding::{self, feature, Multipass, QualityPreset, RateControl};
use ebs_engine::muxer::ContainerFormat;
use ebs_engine::scene::{FitMode, Scene, SceneItem};
use ebs_engine::sources::scene_source::SceneSource;
use ebs_engine::sources::wgc::{CaptureRegion, WgcSource};
use ebs_engine::sources::VideoSource;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{channel, Receiver};
use std::sync::Arc;

pub enum DialogResult {
    None,
    Close,
    PromptOk(PromptAction, String),
    ConfirmOk(ConfirmAction),
    AddSource(Arc<dyn VideoSource>, bool),
    SettingsSaved { reprobe: bool },
    Error(String, String),
}

// ---------------------------------------------------------------------- frame

fn meta(d: &Dialog) -> (String, [f32; 2], &'static str, bool) {
    match d {
        Dialog::Prompt { title, .. } => (title.clone(), [380.0, 130.0], "prompt", false),
        Dialog::Confirm { title, .. } => (title.clone(), [420.0, 140.0], "confirm", false),
        Dialog::Message { title, .. } => (title.clone(), [440.0, 160.0], "message", false),
        Dialog::AddSource(_) => ("Add Source".into(), [720.0, 470.0], "addsource", true),
        Dialog::Transform { .. } => ("Transform".into(), [420.0, 520.0], "transform", false),
        Dialog::Settings(_) => ("Settings".into(), [800.0, 640.0], "settings", true),
        Dialog::Benchmark(_) => ("Optimise encoder".into(), [780.0, 520.0], "benchmark", true),
    }
}

pub fn show(app: &mut EbsApp, ctx: &egui::Context) {
    let Some(mut d) = app.dialog.take() else { return };
    let (title, size, key, resizable) = meta(&d);
    let vp = egui::ViewportId::from_hash_of(("ebs-dialog", key, title.clone()));
    if app.dlg_vp.map(|v| v.0) != Some(vp) {
        // Centre over the main window when the dialog first opens.
        let outer = ctx.input(|i| i.viewport().outer_rect);
        let pos = outer.map(|r| r.center() - Vec2::new(size[0], size[1]) / 2.0).unwrap_or(egui::pos2(200.0, 150.0));
        app.dlg_vp = Some((vp, pos));
    }
    // Clicking the (disabled) main window brings the dialog back to the front.
    let focused = ctx.input(|i| i.viewport().focused).unwrap_or(false);
    if focused && !app.main_focused_last {
        ctx.send_viewport_cmd_to(vp, egui::ViewportCommand::Focus);
    }
    app.main_focused_last = focused;
    let pos = app.dlg_vp.unwrap().1;
    let builder = egui::ViewportBuilder::default()
        .with_title(title)
        .with_inner_size(size)
        .with_position(pos)
        .with_resizable(resizable)
        .with_minimize_button(false)
        .with_icon(crate::icon());
    let res = ctx.show_viewport_immediate(vp, builder, |ctx, _class| {
        let mut res = DialogResult::None;
        egui::CentralPanel::default().frame(egui::Frame::NONE.fill(crate::app::BG).inner_margin(egui::Margin::same(14))).show(ctx, |ui| {
            res = render(app, &mut d, ui);
        });
        if ctx.input(|i| i.viewport().close_requested()) {
            match &mut d {
                Dialog::Benchmark(b) if b.running() => {
                    b.cancel();
                    ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
                }
                Dialog::Settings(_) | Dialog::Transform { .. } | Dialog::Benchmark(_) => res = DialogResult::Close,
                _ => res = DialogResult::Close,
            }
        }
        if ctx.input(|i| i.key_pressed(egui::Key::Escape)) && !matches!(d, Dialog::Benchmark(ref b) if b.running()) {
            res = DialogResult::Close;
        }
        res
    });
    match res {
        DialogResult::None => {
            if app.dialog.is_none() {
                app.dialog = Some(d);
            }
        }
        other => {
            app.dlg_vp = None;
            // Optimise opened from Settings goes back to Settings when it closes.
            if let (DialogResult::Close, Dialog::Benchmark(b)) = (&other, &mut d) {
                if let Some(mut s) = b.return_to.take() {
                    s.refresh_from(&app.s);
                    app.dialog = Some(Dialog::Settings(s));
                    return;
                }
            }
            handle_dialog_result(app, ctx, other);
        }
    }
}

fn buttons(ui: &mut egui::Ui, ok: &str, ok_enabled: bool) -> (bool, bool) {
    let mut r = (false, false);
    ui.with_layout(egui::Layout::right_to_left(egui::Align::Max), |ui| {
        r.1 = ui.add(egui::Button::new("Cancel").min_size(Vec2::new(90.0, 26.0))).clicked();
        r.0 = ui.add_enabled(ok_enabled, egui::Button::new(ok).min_size(Vec2::new(90.0, 26.0))).clicked();
    });
    r
}

fn render(app: &mut EbsApp, d: &mut Dialog, ui: &mut egui::Ui) -> DialogResult {
    match d {
        Dialog::Prompt { label, text, action, .. } => {
            ui.label(label.as_str());
            let te = ui.add(egui::TextEdit::singleline(text).desired_width(f32::INFINITY));
            if !te.has_focus() && !ui.memory(|m| m.focused().is_some()) {
                te.request_focus();
            }
            let enter = te.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
            ui.add_space(10.0);
            let (ok, cancel) = buttons(ui, "OK", !text.trim().is_empty());
            if (ok || enter) && !text.trim().is_empty() {
                let a = std::mem::replace(action, PromptAction::AddScene);
                return DialogResult::PromptOk(a, text.clone());
            }
            if cancel {
                return DialogResult::Close;
            }
            DialogResult::None
        }
        Dialog::Confirm { message, ok, action, .. } => {
            ui.label(message.as_str());
            ui.add_space(14.0);
            let (o, c) = buttons(ui, ok, true);
            if o {
                let a = std::mem::replace(action, ConfirmAction::RecordLowDisk);
                return DialogResult::ConfirmOk(a);
            }
            if c {
                return DialogResult::Close;
            }
            DialogResult::None
        }
        Dialog::Message { message, .. } => {
            egui::ScrollArea::vertical().max_height(90.0).show(ui, |ui| ui.label(message.as_str()));
            ui.add_space(10.0);
            let mut close = false;
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Max), |ui| close = ui.add(egui::Button::new("OK").min_size(Vec2::new(90.0, 26.0))).clicked());
            if close || ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                DialogResult::Close
            } else {
                DialogResult::None
            }
        }
        Dialog::AddSource(s) => s.ui(app, ui),
        Dialog::Transform { item, fields } => transform_ui(app, *item, fields, ui),
        Dialog::Settings(s) => s.ui(app, ui),
        Dialog::Benchmark(b) => b.ui(app, ui),
    }
}

fn row(ui: &mut egui::Ui, label: &str, tip: &str, add: impl FnOnce(&mut egui::Ui)) {
    ui.horizontal(|ui| {
        let l = ui.add_sized([170.0, 22.0], egui::Label::new(label).wrap());
        if !tip.is_empty() {
            l.on_hover_text(tip);
        }
        ui.scope(|ui| {
            add(ui);
        })
        .response
        .on_hover_text(tip);
    });
}

fn section(ui: &mut egui::Ui, t: &str) {
    ui.add_space(8.0);
    ui.label(RichText::new(t).strong().color(crate::app::TEXT));
    ui.separator();
}

fn note(ui: &mut egui::Ui, t: &str) {
    ui.label(RichText::new(t).color(MUTED).size(12.0));
}

fn combo<T: PartialEq + Clone>(ui: &mut egui::Ui, id: &str, value: &mut T, items: &[T], label: impl Fn(&T) -> String) -> bool {
    let before = value.clone();
    crate::widgets::field_style(ui, |ui| {
        egui::ComboBox::from_id_salt(id).width(330.0).icon(crate::widgets::combo_chevron).selected_text(label(value)).show_ui(ui, |ui| {
            for it in items {
                ui.selectable_value(value, it.clone(), label(it));
            }
        })
    });
    *value != before
}

// ---------------------------------------------------------------------- add source

pub struct AddSourceState {
    kind: usize,
    name: String,
    cursor: bool,
    running: Vec<String>,
    game_choices: Vec<String>,
    game: String,
    custom_exe: String,
    windows: Vec<WindowTarget>,
    window: usize,
    monitors: Vec<MonitorTarget>,
    monitor: usize,
    region: [String; 4],
    scenes: Vec<Arc<Scene>>,
    scene: usize,
    existing: Vec<Arc<dyn VideoSource>>,
    existing_pick: usize,
}

const KINDS: [&str; 5] = ["Game Capture", "Window Capture", "Display Capture", "Scene", "Existing Source"];

impl AddSourceState {
    pub fn new(app: &EbsApp) -> AddSourceState {
        let windows = enumerate_windows();
        let mut running: Vec<String> = windows.iter().map(|w| w.exe_name.clone()).collect();
        running.sort_by_key(|s| s.to_lowercase());
        running.dedup_by(|a, b| a.eq_ignore_ascii_case(b));
        let mut choices = vec!["RobloxPlayerBeta".to_string()];
        for g in KNOWN_GAME_EXES.iter().map(|g| g.to_string()).filter(|g| running.iter().any(|r| r.eq_ignore_ascii_case(g))).chain(running.iter().cloned()) {
            if !choices.iter().any(|c| c.eq_ignore_ascii_case(&g)) {
                choices.push(g);
            }
        }
        let monitors = enumerate_monitors();
        let monitor = monitors.iter().position(|m| m.primary).unwrap_or(0);
        let current = app.engine.current_scene();
        // A scene can't contain itself, directly or through other nested scenes.
        let scenes: Vec<Arc<Scene>> = app.coll.scenes.iter().filter(|s| !Arc::ptr_eq(s, &current) && !SceneSource::contains(s, &current, 0)).cloned().collect();
        let in_scene: Vec<u64> = current.snapshot().iter().map(|i| i.source.id()).collect();
        let existing = app
            .engine
            .sources()
            .into_iter()
            .filter(|s| !in_scene.contains(&s.id()) && !s.as_scene_source().and_then(|x| x.inner()).is_some_and(|sc| SceneSource::contains(&sc, &current, 0)))
            .collect();
        AddSourceState {
            kind: 0,
            name: "Roblox".into(),
            cursor: false,
            running,
            game: "RobloxPlayerBeta".into(),
            game_choices: choices,
            custom_exe: String::new(),
            windows,
            window: 0,
            monitors,
            monitor,
            region: Default::default(),
            scenes,
            scene: 0,
            existing,
            existing_pick: 0,
        }
    }

    fn set_kind(&mut self, k: usize) {
        self.kind = k;
        match k {
            0 => {
                self.name = if self.game == "RobloxPlayerBeta" { "Roblox".into() } else { self.game.clone() };
                self.cursor = false;
            }
            1 => {
                self.name = self.windows.get(self.window).map(|w| w.title.clone()).unwrap_or_default();
                self.cursor = true;
            }
            2 => {
                self.name = "Display Capture".into();
                self.cursor = true;
            }
            _ => {}
        }
    }

    fn ui(&mut self, app: &mut EbsApp, ui: &mut egui::Ui) -> DialogResult {
        let mut result = DialogResult::None;
        egui::TopBottomPanel::bottom("as_buttons").frame(egui::Frame::NONE).show_inside(ui, |ui| {
            ui.add_space(8.0);
            let (ok, cancel) = buttons(ui, "Add", true);
            if cancel {
                result = DialogResult::Close;
            }
            if ok {
                result = self.accept(app);
            }
        });
        egui::SidePanel::left("as_types").exact_width(160.0).resizable(false).show_inside(ui, |ui| {
            for (i, k) in KINDS.iter().enumerate() {
                if ui.add_sized([150.0, 24.0], egui::SelectableLabel::new(self.kind == i, *k)).clicked() {
                    self.set_kind(i);
                }
            }
        });
        egui::CentralPanel::default().show_inside(ui, |ui| {
            egui::ScrollArea::vertical().show(ui, |ui| self.page(ui));
        });
        result
    }

    fn page(&mut self, ui: &mut egui::Ui) {
        match self.kind {
            0 => {
                note(ui, "Game Capture follows a program: set it up before or after launching the game, and it re-attaches if the game restarts. It reads the game window through Windows (no code is injected into the game, so anti-cheat such as Roblox's is not triggered).");
                let running = self.running.clone();
                let mut g = self.game.clone();
                row(ui, "Program", "The executable to follow. Roblox is RobloxPlayerBeta.", |ui| {
                    combo(ui, "as_game", &mut g, &self.game_choices, |c| format!("{c}   {}", if running.iter().any(|r| r.eq_ignore_ascii_case(c)) { "(running)" } else { "(not running yet)" }));
                });
                if g != self.game {
                    self.game = g;
                    self.name = if self.game == "RobloxPlayerBeta" { "Roblox".into() } else { self.game.clone() };
                }
                row(ui, "Or type exe name", "Any program name without .exe, e.g. javaw for Minecraft Java.", |ui| {
                    ui.text_edit_singleline(&mut self.custom_exe);
                });
                row(ui, "Name", "", |ui| {
                    ui.text_edit_singleline(&mut self.name);
                });
                crate::widgets::checkbox(ui, &mut self.cursor, "Capture mouse cursor");
            }
            1 => {
                note(ui, "Captures one window, even when other windows cover it. Minimised windows can't be captured.");
                if self.windows.is_empty() {
                    note(ui, "No capturable windows are open.");
                    return;
                }
                let titles: Vec<usize> = (0..self.windows.len()).collect();
                let mut w = self.window;
                let wins = &self.windows;
                row(ui, "Window", "", |ui| {
                    combo(ui, "as_win", &mut w, &titles, |i| format!("[{}] {}", wins[*i].exe_name, trim(&wins[*i].title, 70)));
                });
                if w != self.window {
                    self.window = w;
                    self.name = self.windows[w].title.clone();
                }
                row(ui, "Name", "", |ui| {
                    ui.text_edit_singleline(&mut self.name);
                });
                crate::widgets::checkbox(ui, &mut self.cursor, "Capture mouse cursor");
            }
            2 => {
                if self.monitors.is_empty() {
                    note(ui, "No display found.");
                    return;
                }
                let idx: Vec<usize> = (0..self.monitors.len()).collect();
                let mons = &self.monitors;
                row(ui, "Display", "", |ui| {
                    combo(ui, "as_mon", &mut self.monitor, &idx, |i| mons[*i].to_string());
                });
                row(ui, "Name", "", |ui| {
                    ui.text_edit_singleline(&mut self.name);
                });
                crate::widgets::checkbox(ui, &mut self.cursor, "Capture mouse cursor");
                section(ui, "Custom capture region (optional)");
                note(ui, "Leave at 0 to capture the whole display. Values are pixels on that display.");
                for (i, l) in ["Left", "Top", "Width", "Height"].iter().enumerate() {
                    row(ui, l, "", |ui| {
                        ui.add(egui::TextEdit::singleline(&mut self.region[i]).desired_width(80.0).hint_text("0"));
                    });
                }
            }
            3 => {
                note(ui, "Shows another scene inside this one (a nested scene). Changes to that scene appear everywhere it is used, e.g. one 'Overlay' scene reused in every gameplay scene. Empty areas of the nested scene are transparent.");
                if self.scenes.is_empty() {
                    note(ui, "There is no other scene that can be added here. Add another scene first.");
                    return;
                }
                let idx: Vec<usize> = (0..self.scenes.len()).collect();
                let sc = &self.scenes;
                row(ui, "Scene", "", |ui| {
                    combo(ui, "as_scene", &mut self.scene, &idx, |i| sc[*i].name());
                });
            }
            _ => {
                note(ui, "Adds a source that already exists in another scene. It shares the same capture (no extra cost), with its own position in this scene.");
                if self.existing.is_empty() {
                    note(ui, "Every existing source is already in this scene.");
                    return;
                }
                let idx: Vec<usize> = (0..self.existing.len()).collect();
                let ex = &self.existing;
                row(ui, "Source", "", |ui| {
                    combo(ui, "as_existing", &mut self.existing_pick, &idx, |i| format!("{}  ({})", ex[*i].name(), ex[*i].kind()));
                });
            }
        }
    }

    fn name_or(&self, fallback: &str) -> String {
        if self.name.trim().is_empty() {
            fallback.into()
        } else {
            self.name.trim().into()
        }
    }

    fn accept(&mut self, app: &EbsApp) -> DialogResult {
        let gpu = app.engine.gpu.clone();
        match self.kind {
            0 => {
                let exe = if self.custom_exe.trim().is_empty() { self.game.clone() } else { self.custom_exe.trim().to_string() };
                if exe.is_empty() {
                    return DialogResult::None;
                }
                let exe = exe.strip_suffix(".exe").unwrap_or(&exe).to_string();
                let name = self.name_or(&exe);
                DialogResult::AddSource(Arc::new(WgcSource::game(gpu, &exe, self.cursor, Some(&name))), true)
            }
            1 => match self.windows.get(self.window) {
                Some(w) => {
                    if !ebs_engine::capture::is_window(w.hwnd) {
                        return DialogResult::Error("Add Source".into(), "That window has closed.".into());
                    }
                    let name = self.name_or(&w.title);
                    DialogResult::AddSource(Arc::new(WgcSource::window(gpu, w, self.cursor, Some(&name))), true)
                }
                None => DialogResult::None,
            },
            2 => {
                let Some(m) = self.monitors.get(self.monitor) else { return DialogResult::None };
                let name = self.name_or(&format!("Display {}", m.index + 1));
                let s = WgcSource::monitor(gpu, m, self.cursor, Some(&name));
                let v: Vec<u32> = self.region.iter().map(|t| t.trim().parse().unwrap_or(0)).collect();
                if v[2] > 0 && v[3] > 0 {
                    s.set_region(CaptureRegion { x: v[0], y: v[1], width: v[2], height: v[3] });
                }
                DialogResult::AddSource(Arc::new(s), true)
            }
            3 => {
                let Some(sc) = self.scenes.get(self.scene) else { return DialogResult::None };
                // One source per nested scene: it is drawn once per frame however many scenes show it.
                if let Some(existing) = app.engine.sources().into_iter().find(|s| s.as_scene_source().and_then(|x| x.inner()).is_some_and(|x| Arc::ptr_eq(&x, sc))) {
                    return DialogResult::AddSource(existing, false);
                }
                DialogResult::AddSource(Arc::new(SceneSource::new(Some(sc.clone()), None)), true)
            }
            _ => match self.existing.get(self.existing_pick) {
                Some(s) => DialogResult::AddSource(s.clone(), false),
                None => DialogResult::None,
            },
        }
    }
}

fn trim(s: &str, n: usize) -> String {
    if s.chars().count() > n {
        s.chars().take(n).collect::<String>() + "\u{2026}"
    } else {
        s.into()
    }
}

// ---------------------------------------------------------------------- transform

pub struct TransformFields {
    f: [String; 9],
    opacity: f32,
}

fn num(v: f32) -> String {
    let r = (v * 100.0).round() / 100.0;
    if r.fract() == 0.0 {
        format!("{r:.0}")
    } else {
        format!("{r}")
    }
}

impl TransformFields {
    pub fn load(it: &SceneItem) -> TransformFields {
        TransformFields {
            f: [
                num(it.x),
                num(it.y),
                num(it.display_width()),
                num(it.display_height()),
                num(it.rotation),
                it.crop_left.to_string(),
                it.crop_top.to_string(),
                it.crop_right.to_string(),
                it.crop_bottom.to_string(),
            ],
            opacity: it.opacity * 100.0,
        }
    }

    fn apply(&self, it: &mut SceneItem) {
        let n = |i: usize| self.f[i].trim().parse::<f64>().ok();
        let (sw, sh) = it.source.size();
        if let Some(v) = n(5) {
            it.crop_left = (v.max(0.0) as u32).min(sw.saturating_sub(1));
        }
        if let Some(v) = n(6) {
            it.crop_top = (v.max(0.0) as u32).min(sh.saturating_sub(1));
        }
        if let Some(v) = n(7) {
            it.crop_right = (v.max(0.0) as u32).min(sw.saturating_sub(it.crop_left + 1));
        }
        if let Some(v) = n(8) {
            it.crop_bottom = (v.max(0.0) as u32).min(sh.saturating_sub(it.crop_top + 1));
        }
        if let Some(v) = n(0) {
            it.x = v as f32;
        }
        if let Some(v) = n(1) {
            it.y = v as f32;
        }
        if let Some(v) = n(2).filter(|v| *v > 0.0 && it.cropped_width() > 0) {
            it.scale_x = v as f32 / it.cropped_width() as f32;
        }
        if let Some(v) = n(3).filter(|v| *v > 0.0 && it.cropped_height() > 0) {
            it.scale_y = v as f32 / it.cropped_height() as f32;
        }
        if let Some(v) = n(4) {
            it.rotation = (v % 360.0) as f32;
        }
        it.opacity = self.opacity / 100.0;
        it.auto_fit = false;
    }
}

fn transform_ui(app: &mut EbsApp, item: u64, t: &mut TransformFields, ui: &mut egui::Ui) -> DialogResult {
    let scene = app.engine.current_scene();
    let Some(it) = scene.item(item) else { return DialogResult::Close };
    let c = app.engine.canvas();
    ui.heading(format!("Transform: {}", it.source.name()));
    let (sw, sh) = it.source.size();
    note(ui, &format!("Source size {sw}x{sh}. Canvas {}x{}. Values are canvas pixels.", c.width, c.height));
    let mut changed = false;
    let labels = [
        ("X", "Left edge on the canvas (before rotation)."),
        ("Y", "Top edge on the canvas (before rotation)."),
        ("Width", "Displayed width in canvas pixels (after crop)."),
        ("Height", "Displayed height in canvas pixels (after crop)."),
        ("Rotation (degrees)", "Clockwise rotation around the item's centre."),
        ("Left", ""),
        ("Top", ""),
        ("Right", ""),
        ("Bottom", ""),
    ];
    for (i, (l, tip)) in labels.iter().enumerate() {
        if i == 0 {
            section(ui, "Position and size");
        }
        if i == 5 {
            section(ui, "Crop (source pixels)");
        }
        row(ui, l, tip, |ui| {
            changed |= ui.add(egui::TextEdit::singleline(&mut t.f[i]).desired_width(110.0)).changed();
        });
    }
    section(ui, "Blending");
    row(ui, "Opacity", "100 = fully visible.", |ui| {
        changed |= ui.add(egui::Slider::new(&mut t.opacity, 0.0..=100.0).fixed_decimals(0)).changed();
    });
    if changed {
        scene.update(item, |i| t.apply(i));
    }
    ui.add_space(12.0);
    let mut res = DialogResult::None;
    let mut reload = false;
    ui.horizontal(|ui| {
        if ui.button("Reset").clicked() {
            scene.update(item, |i| {
                (i.crop_left, i.crop_top, i.crop_right, i.crop_bottom) = (0, 0, 0, 0);
                i.rotation = 0.0;
                i.opacity = 1.0;
                i.scale_x = 1.0;
                i.scale_y = 1.0;
                i.x = 0.0;
                i.y = 0.0;
                i.auto_fit = false;
            });
            reload = true;
        }
        for (label, fit, tip) in [("Fit to screen", FitMode::Contain, "Whole picture visible, bars where the shape differs."), ("Fill screen", FitMode::Fill, "Cover the whole canvas; edges that do not fit are cut off (best for Shorts).")] {
            if ui.button(label).on_hover_text(tip).clicked() {
                scene.update(item, |i| {
                    i.rotation = 0.0;
                    i.fit = fit;
                    i.fit_to_canvas(c.width, c.height);
                    i.auto_fit = true;
                });
                reload = true;
            }
        }
        if ui.button("Close").clicked() {
            res = DialogResult::Close;
        }
    });
    if reload {
        if let Some(i) = scene.item(item) {
            *t = TransformFields::load(&i);
        }
    }
    if !matches!(res, DialogResult::None) {
        app.save_scenes();
    }
    res
}

// ---------------------------------------------------------------------- settings

const PAGES: [&str; 5] = ["Output", "Video", "Encoder", "Audio", "Preview"];
const BASE_RES: [(u32, u32); 11] = [(3840, 2160), (2560, 1440), (1920, 1080), (1600, 900), (1280, 720), (854, 480), (2160, 3840), (1440, 2560), (1080, 1920), (720, 1280), (1080, 1080)];
const RATES: [(u32, u32, &str); 6] = [(30, 1, "30"), (30000, 1001, "29.97"), (60, 1, "60"), (60000, 1001, "59.94 (matches a 59.94 Hz display exactly)"), (120, 1, "120"), (144, 1, "144")];

#[derive(Clone, Copy, PartialEq)]
enum PresetChoice {
    Preset(QualityPreset),
    Custom,
}

pub struct SettingsState {
    page: usize,
    d: AppSettings,
    initial_preset: Option<QualityPreset>,
    choice: PresetChoice,
    out_dir: String,
    quality: String,
    bitrate: String,
    maxrate: String,
    keyint: String,
    offset: String,
    reprobe: bool,
    res_list: Vec<(u32, u32)>,
    error: Option<String>,
}

fn res_label(r: &(u32, u32)) -> String {
    let (w, h) = *r;
    let kind = if w > h && w as u64 * 9 == h as u64 * 16 {
        "16:9".to_string()
    } else if h > w && w as u64 * 16 == h as u64 * 9 {
        "9:16 vertical".into()
    } else if w == h {
        "1:1 square".into()
    } else {
        format!("{:.2}:1", w as f64 / h as f64)
    };
    format!("{w}x{h}  ({kind})")
}

impl SettingsState {
    pub fn new(s: &AppSettings, page: Option<&str>) -> SettingsState {
        let mut res_list: Vec<(u32, u32)> = BASE_RES.to_vec();
        for m in enumerate_monitors() {
            let r = (m.width as u32, m.height as u32);
            if !res_list.contains(&r) {
                res_list.push(r);
            }
        }
        for r in [(s.canvas_width, s.canvas_height), (s.output_width, s.output_height)] {
            if !res_list.contains(&r) {
                res_list.push(r);
            }
        }
        res_list.sort_by_key(|&(w, h)| (if h > w { 1 } else if w == h { 2 } else { 0 }, std::cmp::Reverse(w as u64 * h as u64)));
        SettingsState {
            page: page.and_then(|p| PAGES.iter().position(|x| *x == p)).unwrap_or(0),
            initial_preset: s.preset(),
            choice: s.preset().map(PresetChoice::Preset).unwrap_or(PresetChoice::Custom),
            out_dir: s.output_dir.display().to_string(),
            quality: s.video.quality.to_string(),
            bitrate: s.video.bitrate_kbps.to_string(),
            maxrate: s.video.max_bitrate_kbps.to_string(),
            keyint: s.video.keyframe_seconds.to_string(),
            offset: s.audio_offset_ms.to_string(),
            reprobe: false,
            res_list,
            error: None,
            d: s.clone(),
        }
    }

    /// After a preset test from inside Settings: take over the new results (and the applied preset unless the user chose one).
    pub fn refresh_from(&mut self, s: &AppSettings) {
        self.d.preset_results = s.preset_results.clone();
        self.d.preset_results_fingerprint = s.preset_results_fingerprint.clone();
        if !s.preset_chosen_by_user {
            if let Some(p) = s.preset() {
                self.choice = PresetChoice::Preset(p);
                self.d.encoder_preset = s.encoder_preset.clone();
                self.initial_preset = Some(p);
            }
        }
        self.d.preset_chosen_by_user = s.preset_chosen_by_user;
    }

    fn validate(&mut self) -> Option<String> {
        if self.out_dir.trim().is_empty() {
            return Some("Choose a recording folder.".into());
        }
        if self.d.filename_format.trim().is_empty() {
            return Some("Enter a file name pattern, for example yyyy-MM-dd HH-mm-ss.".into());
        }
        if self.d.filename_format.contains(['/', '\\', ':', '*', '?', '"', '<', '>', '|']) {
            return Some("The file name pattern has characters Windows does not allow in file names (such as / or :). Use - instead, for example yyyy-MM-dd HH-mm-ss.".into());
        }
        if self.choice == PresetChoice::Custom {
            match self.quality.trim().parse::<i32>() {
                Ok(q) if (0..=51).contains(&q) => self.d.video.quality = q,
                _ => return Some("Quality level must be between 0 and 51.".into()),
            }
            match self.bitrate.trim().parse::<u32>() {
                Ok(b) if b >= 500 => self.d.video.bitrate_kbps = b,
                _ => return Some("Bitrate must be at least 500 kbps.".into()),
            }
            match self.maxrate.trim().parse::<u32>() {
                Ok(b) if b >= 500 => self.d.video.max_bitrate_kbps = b,
                _ => return Some("Max bitrate must be at least 500 kbps.".into()),
            }
            match self.keyint.trim().parse::<f64>() {
                Ok(k) if k > 0.0 && k <= 20.0 => self.d.video.keyframe_seconds = k,
                _ => return Some("Keyframe interval must be between 0.1 and 20 seconds.".into()),
            }
        }
        if self.d.audio_tracks == 0 {
            return Some("Record at least one audio track.".into());
        }
        match self.offset.trim().parse::<i32>() {
            Ok(o) if o.abs() <= 2000 => self.d.audio_offset_ms = o,
            _ => return Some("Audio offset must be between -2000 and 2000 ms.".into()),
        }
        if self.d.audio_codec == "libopus" && self.d.container != ContainerFormat::Mkv {
            return Some("Opus audio needs the MKV container.".into());
        }
        None
    }

    fn ui(&mut self, app: &mut EbsApp, ui: &mut egui::Ui) -> DialogResult {
        let mut result = DialogResult::None;
        egui::TopBottomPanel::bottom("set_buttons").frame(egui::Frame::NONE).show_inside(ui, |ui| {
            ui.add_space(8.0);
            if let Some(e) = &self.error {
                ui.label(RichText::new(e).color(WARN));
            }
            let (ok, cancel) = buttons(ui, "OK", true);
            if cancel {
                result = DialogResult::Close;
            }
            if ok {
                match self.validate() {
                    Some(e) => self.error = Some(e),
                    None => {
                        self.d.output_dir = self.out_dir.trim().into();
                        self.d.filename_format = self.d.filename_format.trim().into();
                        let chosen = match self.choice {
                            PresetChoice::Preset(p) => Some(p),
                            PresetChoice::Custom => None,
                        };
                        if chosen != self.initial_preset {
                            self.d.preset_chosen_by_user = true;
                        }
                        self.d.encoder_preset = chosen.map(|p| p.label().to_string());
                        if self.d.container != ContainerFormat::Mkv {
                            self.d.remux_to_mp4 = false;
                        }
                        // Probe results may have been refreshed while the dialog was open.
                        let probe = std::mem::take(&mut app.s.probe);
                        let fp = app.s.probe_fingerprint.take();
                        app.s = self.d.clone();
                        app.s.probe = probe;
                        app.s.probe_fingerprint = fp;
                        result = DialogResult::SettingsSaved { reprobe: self.reprobe };
                    }
                }
            }
        });
        egui::SidePanel::left("set_nav").exact_width(130.0).resizable(false).show_inside(ui, |ui| {
            for (i, p) in PAGES.iter().enumerate() {
                if ui.add_sized([120.0, 24.0], egui::SelectableLabel::new(self.page == i, *p)).clicked() {
                    self.page = i;
                }
            }
        });
        egui::CentralPanel::default().show_inside(ui, |ui| {
            egui::ScrollArea::vertical().auto_shrink([false; 2]).show(ui, |ui| match self.page {
                0 => self.output_page(ui),
                1 => self.video_page(ui),
                2 => {
                    if let Some(r) = self.encoder_page(app, ui) {
                        result = r;
                    }
                }
                3 => self.audio_page(ui),
                _ => self.preview_page(ui),
            });
        });
        result
    }

    fn output_page(&mut self, ui: &mut egui::Ui) {
        section(ui, "Recording");
        row(ui, "Recording folder", "Where recordings are saved. Pick a drive with lots of free space: 1080p60 uses about 150-300 MB per minute.", |ui| {
            ui.add(egui::TextEdit::singleline(&mut self.out_dir).desired_width(260.0));
            if ui.button("Browse...").clicked() {
                if let Some(p) = pick_folder(&self.out_dir) {
                    self.out_dir = p;
                }
            }
        });
        row(ui, "File name", "Date/time pattern: yyyy MM dd HH mm ss are replaced with the current time.", |ui| {
            ui.add(egui::TextEdit::singleline(&mut self.d.filename_format).desired_width(260.0));
        });
        let sample = ebs_engine::recorder::format_filename(&self.d.filename_format);
        note(ui, &format!("Next file: {sample}{}", self.d.container.extension()));
        row(ui, "Container", "MKV keeps everything recorded so far if the game, PC or app crashes. MP4 is only readable once it is finished properly.", |ui| {
            combo(ui, "container", &mut self.d.container, &[ContainerFormat::Mkv, ContainerFormat::Mp4, ContainerFormat::Mov], |c| {
                match c {
                    ContainerFormat::Mkv => "MKV (recommended: survives crashes)",
                    ContainerFormat::Mp4 => "MP4 (lost if the PC crashes mid-recording)",
                    ContainerFormat::Mov => "MOV",
                }
                .into()
            });
        });
        ui.add_enabled(self.d.container == ContainerFormat::Mkv, egui::Checkbox::new(&mut self.d.remux_to_mp4, "Automatically convert to MP4 when recording stops (keeps the MKV too)"))
            .on_hover_text("Copies the finished MKV into an MP4 without re-encoding (no quality loss, takes seconds).");
    }

    fn video_page(&mut self, ui: &mut egui::Ui) {
        section(ui, "Mode");
        let mut mode = self.d.video_mode();
        row(ui, "Video mode", "YouTube Wide video: landscape 16:9 (1920x1080). YouTube Short: vertical 9:16 (1080x1920); the game fills the tall frame and its left/right edges are cut off. Custom: pick any sizes below.", |ui| {
            combo(ui, "vmode", &mut mode, &VideoMode::ALL, |m| m.label().into());
        });
        if mode != self.d.video_mode() {
            self.d.apply_mode(mode);
        }
        section(ui, "Canvas and output");
        let list = self.res_list.clone();
        let mut c = (self.d.canvas_width, self.d.canvas_height);
        let mut o = (self.d.output_width, self.d.output_height);
        row(ui, "Base (canvas) resolution", "The size of the scene you arrange sources on. Usually your monitor resolution, or 1080x1920 for a Short.", |ui| {
            combo(ui, "canvas", &mut c, &list, res_label);
        });
        row(ui, "Output (scaled) resolution", "The size of the recorded video. Lower than the canvas = scaled down on the GPU, smaller files and less encoder work.", |ui| {
            combo(ui, "output", &mut o, &list, res_label);
        });
        (self.d.canvas_width, self.d.canvas_height, self.d.output_width, self.d.output_height) = (c.0, c.1, o.0, o.1);
        if c.0 as u64 * o.1 as u64 != c.1 as u64 * o.0 as u64 {
            ui.label(RichText::new(format!("The output shape ({}x{}) differs from the canvas ({}x{}), so the video will be stretched.", o.0, o.1, c.0, c.1)).color(WARN));
        }
        let mut r = RATES.iter().position(|r| r.0 == self.d.fps_num && r.1 == self.d.fps_den).unwrap_or(2);
        let idx: Vec<usize> = (0..RATES.len()).collect();
        row(ui, "Frame rate", "Frames per second. 60 is smooth for gameplay. If your monitor runs at 59.94 Hz, 59.94 avoids one repeated frame every ~17 seconds.", |ui| {
            combo(ui, "fps", &mut r, &idx, |i| RATES[*i].2.into());
        });
        (self.d.fps_num, self.d.fps_den) = (RATES[r].0, RATES[r].1);
        if let Some(hz) = ebs_engine::hardware::active_refresh_rates().first() {
            note(ui, &format!("Your display currently runs at {:.3} Hz.", hz.hz()));
        }
        section(ui, "Colour");
        row(ui, "Colour space", "How colours are converted to video. Rec. 709 is correct for HD video and what players expect.", |ui| {
            combo(ui, "cspace", &mut self.d.color_space_601, &[false, true], |v| if *v { "Rec. 601 (SD)".into() } else { "Rec. 709 (HD, recommended)".into() });
        });
        row(ui, "Colour range", "Limited is what nearly every player and site expects. Full can look washed out or crushed in some players.", |ui| {
            combo(ui, "crange", &mut self.d.color_range_full, &[false, true], |v| if *v { "Full".into() } else { "Limited (recommended)".into() });
        });
    }

    fn encoder_page(&mut self, app: &mut EbsApp, ui: &mut egui::Ui) -> Option<DialogResult> {
        let mut out = None;
        section(ui, "Encoder");
        let working = self.d.working_encoders();
        if working.is_empty() {
            note(ui, "No encoders have been verified yet. Use 'Re-test encoders' below.");
        }
        let ids: Vec<String> = working.iter().map(|e| e.id.to_string()).collect();
        let mut id = self.d.video.encoder_id.clone();
        row(ui, "Video encoder", "Only encoders that were tested and worked on this PC are listed. NVIDIA NVENC uses a dedicated chip on the graphics card, so it barely affects game performance.", |ui| {
            combo(ui, "encoder", &mut id, &ids, |i| {
                let e = encoding::get(i);
                format!("{}{}", e.display_name, if e.hardware { "" } else { "  (uses the CPU: costs game FPS)" })
            });
        });
        if id != self.d.video.encoder_id {
            let e = encoding::get(&id);
            self.d.video.encoder_id = id.clone();
            self.d.video.preset = Some(e.default_preset.into());
            if !e.rate_controls.contains(&self.d.video.rate_control) {
                self.d.video.rate_control = e.rate_controls[0];
            }
        }
        let e = encoding::get(&self.d.video.encoder_id);
        if let Some(p) = self.d.probe.iter().find(|p| p.encoder_id == e.id) {
            note(ui, &format!("Tested on this PC: {}.", p.note));
        }
        let rec = self.d.recommended_preset(Some(e.id));
        let mut choices: Vec<PresetChoice> = QualityPreset::ALL.iter().map(|p| PresetChoice::Preset(*p)).collect();
        choices.push(PresetChoice::Custom);
        let d = &self.d;
        row(ui, "Preset", "SMOOTH / BALANCED / QUALITY are ready-made setups. Each is only marked as tested once it has recorded smoothly on this PC (use 'Test presets'). Custom shows every encoder option.", |ui| {
            combo(ui, "qpreset", &mut self.choice, &choices, |c| match c {
                PresetChoice::Preset(p) => {
                    let status = match d.result_for(*p, Some(e.id)) {
                        None => "not tested yet",
                        Some(r) if r.passed => "tested smooth on this PC",
                        Some(_) => "not smooth on this PC",
                    };
                    format!("{}{}  -  {status}", p.label(), if rec == Some(*p) { " (recommended)" } else { "" })
                }
                PresetChoice::Custom => "Custom (choose every option)".into(),
            });
        });
        match self.choice {
            PresetChoice::Preset(p) => {
                let v = self.d.effective_video_for(e.id, Some(p));
                let mut uses = format!(
                    "Uses: {}, constant quality {}, max {} Mbps{}",
                    e.preset_label(v.preset.as_deref().unwrap_or(e.default_preset)),
                    v.quality,
                    v.max_bitrate_kbps / 1000,
                    if v.b_frames > 0 { format!(", {} B-frames", v.b_frames) } else { ", no B-frames".into() }
                );
                if v.look_ahead > 0 {
                    uses += &format!(", look-ahead {}", v.look_ahead);
                }
                if v.spatial_aq {
                    uses += ", spatial AQ";
                }
                if v.temporal_aq {
                    uses += ", temporal AQ";
                }
                if v.multipass != Multipass::Disabled {
                    uses += ", two-pass (quarter res)";
                }
                let test = match self.d.result_for(p, Some(e.id)) {
                    None => format!("Not tested at {}x{} {} fps on this PC yet.", self.d.output_width, self.d.output_height, crate::app::fmt_fps(self.d.fps_num, self.d.fps_den)),
                    Some(r) => format!("{}{} ({}).", if r.passed { "Tested smooth on this PC: " } else { "Not smooth on this PC: " }, r.verdict, r.content),
                };
                note(ui, &format!("{}\n{uses}.\n{test}", p.describe()));
            }
            PresetChoice::Custom => note(ui, "Custom: every setting below is used exactly as you set it."),
        }
        if ui.button("Test presets on this PC...").on_hover_text("Records a short test clip with each preset and picks the best one that stays smooth (about a minute).").clicked() {
            if app.is_recording() {
                self.error = Some("Stop the recording before testing presets.".into());
            } else {
                out = Some(DialogResult::None);
                let mut b = BenchState::new(&app.s, e.id);
                // The test runs at the resolution and rate chosen here; Settings reopens afterwards.
                b.s = self.d.clone();
                b.s.probe = app.s.probe.clone();
                b.return_to = Some(Box::new(std::mem::replace(self, SettingsState::new(&app.s, None))));
                b.return_to.as_mut().unwrap().page = 2;
                app.dialog = Some(Dialog::Benchmark(Box::new(b)));
                app.dlg_vp = None;
                return out;
            }
        }
        if self.choice == PresetChoice::Custom {
            let feats = self.d.verified_features(e.id);
            let v = &mut self.d.video;
            section(ui, "Custom settings");
            row(ui, "Rate control", "Constant quality: keeps detail steady and spends more bits only on busy scenes (best for recordings). CQP: fixed quantiser. VBR/CBR: target a bitrate.", |ui| {
                combo(ui, "rc", &mut v.rate_control, e.rate_controls, |r| {
                    match r {
                        RateControl::ConstantQuality => "Constant quality (recommended)",
                        RateControl::Cqp => "CQP (fixed quantiser)",
                        RateControl::Vbr => "VBR (variable bitrate)",
                        RateControl::Cbr => "CBR (constant bitrate)",
                    }
                    .into()
                });
            });
            let rc = v.rate_control;
            if matches!(rc, RateControl::ConstantQuality | RateControl::Cqp) {
                row(ui, "Quality level", "Lower = sharper and bigger files. NVENC CQ 16-18 is near-lossless for gameplay, 20-23 is a good balance, 26+ starts to look soft.", |ui| {
                    ui.add(egui::TextEdit::singleline(&mut self.quality).desired_width(80.0));
                });
            }
            if matches!(rc, RateControl::Vbr | RateControl::Cbr) {
                row(ui, "Bitrate (kbps)", "Target bitrate. 1080p60 gameplay looks good at 20,000-50,000 kbps.", |ui| {
                    ui.add(egui::TextEdit::singleline(&mut self.bitrate).desired_width(80.0));
                });
            }
            if matches!(rc, RateControl::ConstantQuality | RateControl::Vbr) {
                row(ui, "Max bitrate (kbps)", "Upper limit so a very busy moment can't create a huge spike.", |ui| {
                    ui.add(egui::TextEdit::singleline(&mut self.maxrate).desired_width(80.0));
                });
            }
            let presets: Vec<String> = e.presets.iter().map(|s| s.to_string()).collect();
            let mut p = v.preset.clone().filter(|p| presets.contains(p)).unwrap_or_else(|| e.default_preset.into());
            row(ui, "Preset", "Speed vs quality of the encoder itself. On NVENC this uses the encoder chip, not the CPU or the game's GPU cores.", |ui| {
                combo(ui, "encpreset", &mut p, &presets, |x| e.preset_label(x));
            });
            v.preset = Some(p);
            if !e.tunes.is_empty() && feats & feature::TUNE != 0 {
                let tunes: Vec<String> = e.tunes.iter().map(|s| s.to_string()).collect();
                let mut t = v.tune.clone().filter(|t| tunes.contains(t)).unwrap_or_else(|| tunes[0].clone());
                row(ui, "Tuning", "hq = high quality (recordings). ll/ull = low latency (only matters for streaming).", |ui| {
                    combo(ui, "tune", &mut t, &tunes, |x| x.clone());
                });
                v.tune = Some(t);
            } else {
                v.tune = None;
            }
            let profiles: Vec<String> = e.profiles.iter().filter(|p| **p != "main10").map(|s| s.to_string()).collect();
            if !profiles.is_empty() {
                let mut pr = v.profile.clone().filter(|p| profiles.contains(p)).unwrap_or_else(|| profiles[0].clone());
                row(ui, "Profile", "Feature level of the stream. High (H.264) / Main (HEVC) play everywhere.", |ui| {
                    combo(ui, "profile", &mut pr, &profiles, |x| x.clone());
                });
                v.profile = Some(pr);
            }
            row(ui, "Keyframe interval (s)", "Seconds between full frames. 2 s is standard; makes seeking and editing snappy.", |ui| {
                ui.add(egui::TextEdit::singleline(&mut self.keyint).desired_width(80.0));
            });
            // Only features verified on this PC for this encoder are offered.
            section(ui, "Advanced");
            if feats & feature::B_FRAMES != 0 {
                row(ui, "B-frames", "Frames predicted from both directions. 2 improves quality per bit at almost no cost on this encoder.", |ui| {
                    combo(ui, "bf", &mut v.b_frames, &[0, 1, 2, 3, 4], |x| x.to_string());
                });
            } else {
                v.b_frames = 0;
            }
            if feats & feature::B_FRAME_REFS != 0 {
                row(ui, "B-frames as reference", "Lets other frames reference a B-frame. Better quality in motion.", |ui| {
                    crate::widgets::checkbox(ui, &mut v.b_frame_refs, "Use middle B-frame as reference");
                });
            } else {
                v.b_frame_refs = false;
            }
            if feats & feature::LOOK_AHEAD != 0 {
                row(ui, "Look-ahead (frames)", "Encoder looks this many frames ahead to place bits better. Uses more encoder time and GPU memory; 0 for the lowest impact.", |ui| {
                    combo(ui, "la", &mut v.look_ahead, &[0, 4, 8, 16, 20, 32], |x| x.to_string());
                });
            } else {
                v.look_ahead = 0;
            }
            if feats & feature::SPATIAL_AQ != 0 {
                row(ui, "Spatial AQ", "Adaptive quantisation: reduces blocky skies and gradients. Recommended.", |ui| {
                    crate::widgets::checkbox(ui, &mut v.spatial_aq, "Spend more bits on flat areas");
                });
                let strengths: Vec<i32> = (1..=15).collect();
                row(ui, "AQ strength", "How strongly spatial AQ moves bits around (8 = default).", |ui| {
                    combo(ui, "aqs", &mut v.aq_strength, &strengths, |x| x.to_string());
                });
            } else {
                v.spatial_aq = false;
            }
            if feats & feature::TEMPORAL_AQ != 0 {
                row(ui, "Temporal AQ", "Spends bits on areas that stay still across frames.", |ui| {
                    crate::widgets::checkbox(ui, &mut v.temporal_aq, "Improve static detail over time");
                });
            } else {
                v.temporal_aq = false;
            }
            if feats & feature::MULTIPASS != 0 {
                row(ui, "Multipass", "A quick analysis pass before encoding each frame for better bit distribution. Costs encoder throughput.", |ui| {
                    combo(ui, "mp", &mut v.multipass, &[Multipass::Disabled, Multipass::QuarterRes, Multipass::FullRes], |m| {
                        match m {
                            Multipass::Disabled => "Single pass (lowest impact)",
                            Multipass::QuarterRes => "Two pass, quarter resolution",
                            Multipass::FullRes => "Two pass, full resolution",
                        }
                        .into()
                    });
                });
            } else {
                v.multipass = Multipass::Disabled;
            }
            if feats & feature::PSYCHO_VISUAL != 0 {
                row(ui, "Psycho-visual tuning", "Keeps grain and texture looking natural instead of smoothing it away.", |ui| {
                    crate::widgets::checkbox(ui, &mut v.psycho_visual, "Keep fine texture (x264 psy-rd)");
                });
            }
        }
        ui.add_space(10.0);
        let label = if self.reprobe { "Encoders will be re-tested when you press OK" } else { "Re-test encoders" };
        if ui.button(label).on_hover_text("Runs the encoder test again (e.g. after a driver update).").clicked() {
            self.reprobe = true;
        }
        out
    }

    fn audio_page(&mut self, ui: &mut egui::Ui) {
        section(ui, "Encoding");
        let codecs: Vec<String> = ["aac", "libopus", "flac"].iter().map(|s| s.to_string()).collect();
        row(ui, "Audio codec", "AAC is the safe choice. FLAC keeps audio lossless for editing.", |ui| {
            combo(ui, "acodec", &mut self.d.audio_codec, &codecs, |c| {
                match c.as_str() {
                    "aac" => "AAC (plays everywhere)",
                    "libopus" => "Opus (better quality per bit, MKV only)",
                    _ => "FLAC (lossless, large)",
                }
                .into()
            });
        });
        row(ui, "Bitrate per track", "192 kbps AAC is transparent for game audio and voice. Ignored for FLAC.", |ui| {
            combo(ui, "abr", &mut self.d.audio_bitrate_kbps, &[128, 160, 192, 256, 320], |b| format!("{b} kbps"));
        });
        section(ui, "Tracks");
        note(ui, "Each track is a separate audio stream in the file, so you can balance game and voice later in an editor. Choose which sources go to each track by right-clicking a source in the mixer.");
        let names = ["Track 1: Mix of everything", "Track 2: Desktop / game audio", "Track 3: Microphone", "Track 4", "Track 5", "Track 6"];
        for (t, n) in names.iter().enumerate() {
            let bit = 1u8 << t;
            let mut on = self.d.audio_tracks & bit != 0;
            if crate::widgets::checkbox(ui, &mut on, n).changed() {
                self.d.audio_tracks = if on { self.d.audio_tracks | bit } else { self.d.audio_tracks & !bit };
            }
        }
        section(ui, "Sync");
        crate::widgets::checkbox(ui, &mut self.d.auto_av_sync, "Automatic A/V sync (delay audio by the measured capture latency)")
            .on_hover_text("Video reaches the recorder about one frame after it appears on screen; sound arrives sooner. EBS measures this per recording and lines them up.");
        row(ui, "Extra audio offset (ms)", "Manual fine-tune. Positive = audio later, negative = audio earlier. Leave at 0 unless you hear a delay.", |ui| {
            ui.add(egui::TextEdit::singleline(&mut self.offset).desired_width(80.0));
        });
    }

    fn preview_page(&mut self, ui: &mut egui::Ui) {
        section(ui, "Preview");
        row(ui, "Preview frame rate", "How often the preview refreshes. It never affects the recording; a slower preview leaves more GPU time for the game.", |ui| {
            combo(ui, "pdiv", &mut self.d.preview_fps_divisor, &[1, 2, 4], |d| {
                match d {
                    1 => "Full rate",
                    2 => "Half rate (recommended)",
                    _ => "Quarter rate",
                }
                .into()
            });
        });
        section(ui, "This PC");
        let hw = ebs_engine::hardware::HardwareInfo::detect();
        ui.label(RichText::new(hw.to_string()).monospace().size(11.0).color(MUTED));
    }
}

// ---------------------------------------------------------------------- optimise

enum BenchMsg {
    Progress(BenchProgress),
    Done(anyhow::Result<Vec<PresetTestResult>>),
}

pub struct BenchState {
    pub s: AppSettings,
    encoder_id: String,
    rx: Option<Receiver<BenchMsg>>,
    cancel: Arc<AtomicBool>,
    fraction: f64,
    status: String,
    results: Vec<PresetTestResult>,
    used: bool,
    tested_again: bool,
    pub return_to: Option<Box<SettingsState>>,
}

impl BenchState {
    pub fn new(s: &AppSettings, encoder_id: &str) -> BenchState {
        let results = QualityPreset::ALL.iter().filter_map(|p| s.result_for(*p, Some(encoder_id)).cloned()).collect();
        BenchState {
            s: s.clone(),
            encoder_id: encoder_id.into(),
            rx: None,
            cancel: Arc::new(AtomicBool::new(false)),
            fraction: 0.0,
            status: String::new(),
            results,
            used: false,
            tested_again: false,
            return_to: None,
        }
    }

    pub fn running(&self) -> bool {
        self.rx.is_some()
    }

    pub fn cancel(&self) {
        self.cancel.store(true, Ordering::Relaxed);
    }

    fn temp_dir(&self) -> std::path::PathBuf {
        // On the recordings drive: a 1080p60 test clip can be 100 MB and the system drive may be nearly full.
        self.s.output_dir.join(".ebs-benchmark")
    }

    fn start(&mut self, app: &EbsApp) {
        self.cancel = Arc::new(AtomicBool::new(false));
        self.results.clear();
        self.used = false;
        let mut rs = self.s.to_recording_settings();
        rs.video = self.s.effective_video_for(&self.encoder_id, None);
        let mut b = PresetBenchmark::new(app.engine.shared.clone(), rs, self.s.verified_features(&self.encoder_id));
        b.temp_dir = self.temp_dir();
        b.cancel = self.cancel.clone();
        let (tx, rx) = channel();
        std::thread::spawn(move || {
            let tx2 = tx.clone();
            let r = b.run(&mut |p| {
                let _ = tx2.send(BenchMsg::Progress(p));
            });
            let _ = tx.send(BenchMsg::Done(r));
        });
        self.rx = Some(rx);
        self.status = "Starting...".into();
        self.fraction = 0.0;
    }

    fn poll(&mut self, app: &mut EbsApp) {
        let Some(rx) = &self.rx else { return };
        let mut done = None;
        while let Ok(m) = rx.try_recv() {
            match m {
                BenchMsg::Progress(p) => {
                    self.fraction = p.fraction;
                    self.status = p.message;
                    if let Some(r) = p.completed {
                        self.results.push(r);
                    }
                }
                BenchMsg::Done(r) => done = Some(r),
            }
        }
        let Some(r) = done else { return };
        self.rx = None;
        self.fraction = 1.0;
        self.tested_again = true;
        let _ = std::fs::remove_dir_all(self.temp_dir());
        match r {
            Ok(results) => {
                // Results are stored for the resolution/rate they were measured at (they may come from Settings' draft).
                app.s.store_preset_results(&results);
                self.s.store_preset_results(&results);
                let rec = benchmark::recommend(&results);
                if !app.s.preset_chosen_by_user {
                    app.s.encoder_preset = Some(rec.label().into());
                    self.s.encoder_preset = Some(rec.label().into());
                }
                app.s.save();
                log(format!("benchmark: {}", results.iter().map(|r| format!("{} {} {}", r.preset, if r.passed { "pass" } else { "fail" }, r.verdict)).collect::<Vec<_>>().join(" | ")));
                self.status = format!("Finished. Tested on: {}.", results.first().map(|r| r.content.as_str()).unwrap_or(""));
                self.results = results;
            }
            Err(e) if e.to_string() == "cancelled" => self.status = "Test cancelled. Nothing was changed.".into(),
            Err(e) => {
                log(format!("benchmark failed: {e:#}"));
                self.status = format!("The test could not finish: {e:#}");
            }
        }
    }

    fn ui(&mut self, app: &mut EbsApp, ui: &mut egui::Ui) -> DialogResult {
        self.poll(app);
        let info = encoding::get(&self.encoder_id);
        ui.heading("Find the best smooth preset for this PC");
        ui.add_space(6.0);
        ui.label(format!(
            "Records a short test clip with each preset using {} at {}x{} {} fps, exactly like a real recording, and checks for dropped frames, late frames, encoder load and whether the game's frame rate drops. Takes about a minute. The test clips are deleted afterwards.",
            info.display_name,
            self.s.output_width,
            self.s.output_height,
            crate::app::fmt_fps(self.s.fps_num, self.s.fps_den)
        ));
        note(ui, "For the most accurate result, play your game normally (moving around) while the test runs. If nothing in the scene is moving, a hard-to-encode moving test pattern is used instead.");
        ui.add_space(8.0);
        ui.add(egui::ProgressBar::new(self.fraction as f32).desired_height(6.0));
        ui.label(&self.status);
        ui.add_space(8.0);
        let mut sorted = self.results.clone();
        sorted.sort_by_key(|r| r.preset().map(|p| p as u8).unwrap_or(9));
        egui::Grid::new("bench_results").num_columns(3).spacing([10.0, 6.0]).show(ui, |ui| {
            for r in &sorted {
                ui.label(RichText::new(&r.preset).strong());
                ui.label(RichText::new(if r.passed { "Smooth" } else { "Not smooth" }).strong().color(if r.passed { GOOD } else { WARN }));
                let mut detail = format!("{} frames, {} dropped, {} late", r.frames_encoded, r.frames_dropped, r.render_lag_frames);
                if r.encoder_percent >= 0.0 {
                    detail += &format!(", encoder chip {:.0}% (peak {:.0}%)", r.encoder_percent, r.encoder_percent_peak);
                }
                if r.capture_fps_baseline > 0.0 {
                    detail += &format!(", game {:.0} to {:.0} fps", r.capture_fps_baseline, r.capture_fps_during);
                }
                detail += &format!(", {:.1} Mbps", r.bitrate_mbps);
                if !r.passed {
                    detail += &format!("\n{}", r.verdict);
                }
                ui.add(egui::Label::new(detail).wrap()).on_hover_text(format!("{}\nTested on: {}", r.verdict, r.content));
                ui.end_row();
            }
        });
        let mut rec = None;
        if sorted.len() == QualityPreset::ALL.len() && !self.running() {
            let r = benchmark::recommend(&sorted);
            rec = Some(r);
            let in_use = app.s.preset() == Some(r);
            let text = if sorted.iter().any(|x| x.passed) {
                format!("Recommended: {}: the highest-quality preset that stayed smooth on this PC{}", r.label(), if in_use { " (now in use)." } else { "." })
            } else {
                "No preset stayed fully smooth. SMOOTH has the lowest impact; also consider a lower resolution or 30 fps.".into()
            };
            ui.add_space(8.0);
            ui.label(RichText::new(text).strong());
        }
        ui.add_space(12.0);
        let mut res = DialogResult::None;
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Max), |ui| {
            let close = ui.add(egui::Button::new(if self.running() { "Cancel" } else { "Close" }).min_size(Vec2::new(90.0, 26.0))).clicked();
            if close {
                if self.running() {
                    self.cancel();
                } else {
                    res = DialogResult::Close;
                }
            }
            let start_label = if self.tested_again { "Test again" } else { "Start test" };
            if ui.add_enabled(!self.running(), egui::Button::new(start_label).min_size(Vec2::new(110.0, 26.0))).clicked() {
                self.start(app);
            }
            if let Some(r) = rec {
                if app.s.preset() != Some(r) || self.used {
                    let label = if self.used { format!("Using {}", r.label()) } else { format!("Use {}", r.label()) };
                    if ui.add_enabled(!self.used, egui::Button::new(label).min_size(Vec2::new(150.0, 26.0))).clicked() {
                        app.s.encoder_preset = Some(r.label().into());
                        app.s.preset_chosen_by_user = true;
                        app.s.save();
                        if let Some(s) = self.return_to.as_mut() {
                            s.choice = PresetChoice::Preset(r);
                            s.d.encoder_preset = Some(r.label().into());
                            s.d.preset_chosen_by_user = true;
                        }
                        self.used = true;
                    }
                }
            }
        });
        if self.running() {
            ui.ctx().request_repaint_after(std::time::Duration::from_millis(100));
        }
        res
    }
}

// ---------------------------------------------------------------------- shell helpers

/// Moves a file to the Recycle Bin.
pub fn recycle(path: &Path) -> anyhow::Result<()> {
    use windows::Win32::UI::Shell::*;
    let mut wide: Vec<u16> = path.as_os_str().encode_wide_vec();
    wide.push(0);
    wide.push(0);
    let mut op = SHFILEOPSTRUCTW {
        wFunc: FO_DELETE,
        pFrom: windows::core::PCWSTR(wide.as_ptr()),
        fFlags: (FOF_ALLOWUNDO | FOF_NOCONFIRMATION | FOF_SILENT | FOF_NOERRORUI).0 as u16,
        ..Default::default()
    };
    let r = unsafe { SHFileOperationW(&mut op) };
    if r != 0 {
        anyhow::bail!("error {r}");
    }
    Ok(())
}

trait WideVec {
    fn encode_wide_vec(&self) -> Vec<u16>;
}
impl WideVec for std::ffi::OsStr {
    fn encode_wide_vec(&self) -> Vec<u16> {
        use std::os::windows::ffi::OsStrExt;
        self.encode_wide().collect()
    }
}

/// Windows folder picker. Returns None when cancelled.
fn pick_folder(start: &str) -> Option<String> {
    use windows::Win32::System::Com::*;
    use windows::Win32::UI::Shell::*;
    unsafe {
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
        let dlg: IFileOpenDialog = CoCreateInstance(&FileOpenDialog, None, CLSCTX_INPROC_SERVER).ok()?;
        let opts = dlg.GetOptions().ok()?;
        dlg.SetOptions(opts | FOS_PICKFOLDERS | FOS_FORCEFILESYSTEM).ok()?;
        if Path::new(start).is_dir() {
            if let Ok(item) = SHCreateItemFromParsingName::<_, _, IShellItem>(&windows::core::HSTRING::from(start), None) {
                let _ = dlg.SetFolder(&item);
            }
        }
        dlg.Show(None).ok()?;
        let item = dlg.GetResult().ok()?;
        let p = item.GetDisplayName(SIGDN_FILESYSPATH).ok()?;
        let s = p.to_string().ok();
        CoTaskMemFree(Some(p.0 as _));
        s
    }
}
