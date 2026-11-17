//! Scene collection: scenes, the sources they share, item transforms and audio sources. Saved as JSON
//! (one file per collection), in the same format as the C# EBS so existing collections load unchanged.
use crate::audio::{AudioSource, AudioSourceKind};
use crate::capture::{enumerate_monitors, enumerate_windows, find_window_by_exe};
use crate::engine::{EngineShared, TransitionKind};
use crate::scene::{FitMode, Scene};
use crate::sources::pattern::TestPatternSource;
use crate::sources::scene_source::SceneSource;
use crate::sources::wgc::{CaptureRegion, WgcSource};
use crate::sources::VideoSource;
use anyhow::{anyhow, Result};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::sync::Arc;

pub struct SceneCollection {
    pub dir: PathBuf,
    pub name: String,
    pub scenes: Vec<Arc<Scene>>,
    pub transition: TransitionKind,
    pub transition_ms: u32,
}

/// Collection names are file names: strip characters Windows does not allow.
pub fn safe_name(name: &str) -> String {
    let s: String = name.trim().chars().map(|c| if "<>:\"/\\|?*".contains(c) || c.is_control() { '-' } else { c }).collect();
    let s = s.trim().trim_end_matches('.').to_string();
    if s.is_empty() {
        "Collection".into()
    } else {
        s
    }
}

/// Fit mode a new item gets on this canvas: portrait canvases (YouTube Shorts) are filled, others contained.
pub fn fit_for(cw: u32, ch: u32) -> FitMode {
    if ch > cw {
        FitMode::Fill
    } else {
        FitMode::Contain
    }
}

impl SceneCollection {
    pub fn new(dir: &Path, name: &str) -> Self {
        Self { dir: dir.to_path_buf(), name: name.into(), scenes: Vec::new(), transition: TransitionKind::Fade, transition_ms: 300 }
    }

    pub fn list(dir: &Path) -> Vec<String> {
        let mut v: Vec<String> = std::fs::read_dir(dir)
            .map(|rd| {
                rd.filter_map(|e| e.ok())
                    .filter_map(|e| {
                        let p = e.path();
                        (p.extension().is_some_and(|x| x.eq_ignore_ascii_case("json"))).then(|| p.file_stem().unwrap().to_string_lossy().into_owned())
                    })
                    .collect()
            })
            .unwrap_or_default();
        v.sort_by_key(|s| s.to_lowercase());
        v
    }

    pub fn path_for(dir: &Path, name: &str) -> PathBuf {
        dir.join(format!("{name}.json"))
    }

    /// Removes every video and audio source from the engine (before loading another collection).
    pub fn unload(&mut self, engine: &EngineShared) {
        engine.set_current_scene(Scene::new("Scene"));
        *engine.preview_selection.lock().unwrap() = None;
        for s in engine.sources() {
            engine.remove_source(s.id());
        }
        for a in engine.audio.sources() {
            engine.audio.remove(a.id);
        }
        self.scenes.clear();
    }

    /// Removes a source from every scene and from the engine.
    pub fn remove_source_everywhere(&self, engine: &EngineShared, id: u64) {
        for s in &self.scenes {
            s.remove_source(id);
        }
        engine.remove_source(id);
    }

    pub fn save(&self, engine: &EngineShared) -> Result<()> {
        let mut ids: HashMap<u64, String> = HashMap::new();
        let mut sources = Vec::new();
        for s in engine.sources() {
            let id = format!("s{}", ids.len());
            ids.insert(s.id(), id.clone());
            let mut o = s.save();
            o["id"] = json!(id);
            o["name"] = json!(s.name());
            sources.push(o);
        }
        let scenes: Vec<Value> = self
            .scenes
            .iter()
            .map(|sc| {
                let items: Vec<Value> = sc
                    .snapshot()
                    .iter()
                    .filter_map(|it| {
                        let sid = ids.get(&it.source.id())?;
                        Some(json!({
                            "source": sid, "visible": it.visible, "locked": it.locked, "autoFit": it.auto_fit,
                            "fit": format!("{:?}", it.fit), "x": it.x, "y": it.y, "sx": it.scale_x, "sy": it.scale_y,
                            "rot": it.rotation, "opacity": it.opacity,
                            "crop": [it.crop_left, it.crop_top, it.crop_right, it.crop_bottom],
                        }))
                    })
                    .collect();
                json!({ "name": sc.name(), "items": items })
            })
            .collect();
        let audio: Vec<Value> = engine
            .audio
            .sources()
            .iter()
            .map(|a| {
                json!({
                    "kind": format!("{:?}", a.kind), "device": a.device_id, "name": a.name(),
                    "volume": a.volume.load(), "muted": a.muted.load(Ordering::Relaxed), "tracks": a.track_mask.load(Ordering::Relaxed),
                })
            })
            .collect();
        let root = json!({
            "transition": format!("{:?}", self.transition),
            "transitionMs": self.transition_ms,
            "current": engine.current_scene().name(),
            "sources": sources,
            "scenes": scenes,
            "audio": audio,
        });
        std::fs::create_dir_all(&self.dir)?;
        let path = Self::path_for(&self.dir, &self.name);
        crate::util::write_replace(&path, serde_json::to_string_pretty(&root)?.as_bytes())?;
        Ok(())
    }

    /// Loads a collection into the engine (which must be empty). Returns false when there is no saved collection.
    pub fn load(&mut self, engine: &EngineShared, name: &str, warn: &mut dyn FnMut(String)) -> Result<bool> {
        self.transition = TransitionKind::Fade;
        self.transition_ms = 300;
        let path = Self::path_for(&self.dir, name);
        if !path.exists() {
            return Ok(false);
        }
        self.name = name.into();
        let root: Value = serde_json::from_str(&std::fs::read_to_string(&path)?)?;
        let canvas = engine.canvas();
        let mut map: HashMap<String, Arc<dyn VideoSource>> = HashMap::new();
        let mut nested: Vec<(Arc<dyn VideoSource>, String)> = Vec::new();
        for o in root["sources"].as_array().into_iter().flatten() {
            let nm = o["name"].as_str().unwrap_or("Source").to_string();
            match create_source(engine, o) {
                Ok(src) => {
                    if o["kind"] == "scene" {
                        nested.push((src.clone(), o["scene"].as_str().unwrap_or("").to_string()));
                    }
                    engine.add_source(src.clone());
                    map.insert(o["id"].as_str().unwrap_or("").to_string(), src);
                }
                Err(e) => warn(format!("Source \"{nm}\" could not start: {e}")),
            }
        }
        self.scenes.clear();
        for so in root["scenes"].as_array().into_iter().flatten() {
            let scene = Scene::new(so["name"].as_str().unwrap_or("Scene"));
            for io in so["items"].as_array().into_iter().flatten() {
                let Some(src) = io["source"].as_str().and_then(|s| map.get(s)) else { continue };
                let id = scene.add(src.clone());
                let f = |k: &str, d: f32| io[k].as_f64().map(|v| v as f32).unwrap_or(d);
                scene.update(id, |it| {
                    it.visible = io["visible"].as_bool().unwrap_or(true);
                    it.locked = io["locked"].as_bool().unwrap_or(false);
                    it.auto_fit = io["autoFit"].as_bool().unwrap_or(true);
                    it.fit = match io["fit"].as_str() {
                        Some("Fill") => FitMode::Fill,
                        Some("Contain") => FitMode::Contain,
                        _ => fit_for(canvas.width, canvas.height),
                    };
                    it.x = f("x", 0.0);
                    it.y = f("y", 0.0);
                    it.scale_x = f("sx", 1.0);
                    it.scale_y = f("sy", 1.0);
                    it.rotation = f("rot", 0.0);
                    it.opacity = f("opacity", 1.0);
                    if let Some(c) = io["crop"].as_array().filter(|c| c.len() == 4) {
                        let g = |i: usize| c[i].as_u64().unwrap_or(0) as u32;
                        (it.crop_left, it.crop_top, it.crop_right, it.crop_bottom) = (g(0), g(1), g(2), g(3));
                    }
                });
            }
            self.scenes.push(scene);
        }
        if self.scenes.is_empty() {
            self.scenes.push(Scene::new("Scene"));
        }
        // Nested scenes refer to scenes by name, so they are connected once every scene exists.
        for (src, sname) in nested {
            if let Some(ss) = src.as_scene_source() {
                *ss.scene.lock().unwrap() = self.scenes.iter().find(|s| s.name() == sname).cloned();
            }
        }
        if let Some(k) = root["transition"].as_str().and_then(TransitionKind::parse) {
            self.transition = k;
            self.transition_ms = root["transitionMs"].as_u64().unwrap_or(300) as u32;
        }
        let current = root["current"].as_str().unwrap_or("");
        engine.set_current_scene(self.scenes.iter().find(|s| s.name() == current).cloned().unwrap_or_else(|| self.scenes[0].clone()));

        for a in root["audio"].as_array().into_iter().flatten() {
            let kind = match a["kind"].as_str() {
                Some("Microphone") => AudioSourceKind::Microphone,
                Some("DesktopAudio") => AudioSourceKind::DesktopAudio,
                other => {
                    warn(format!("Unknown audio source kind {other:?}"));
                    continue;
                }
            };
            let dev = a["device"].as_str().map(String::from);
            let nm = a["name"].as_str().unwrap_or(if kind == AudioSourceKind::Microphone { "Microphone" } else { "Desktop Audio" }).to_string();
            let clock = engine.audio.clock;
            let src = if kind == AudioSourceKind::Microphone { AudioSource::microphone(clock, dev, &nm) } else { AudioSource::desktop(clock, dev, &nm) };
            src.volume.store(a["volume"].as_f64().unwrap_or(1.0) as f32);
            src.muted.store(a["muted"].as_bool().unwrap_or(false), Ordering::Relaxed);
            if let Some(t) = a["tracks"].as_u64() {
                src.track_mask.store(t as u32, Ordering::Relaxed);
            }
            engine.audio.add(src);
        }
        Ok(true)
    }

    /// First launch: Roblox game capture, desktop audio and microphone.
    pub fn create_default(&mut self, engine: &EngineShared) {
        self.scenes.clear();
        let scene = Scene::new("Gameplay");
        self.scenes.push(scene.clone());
        engine.set_current_scene(scene.clone());
        let game: Arc<dyn VideoSource> = Arc::new(WgcSource::game(engine.gpu.clone(), "RobloxPlayerBeta", false, Some("Roblox")));
        engine.add_source(game.clone());
        let id = scene.add(game);
        let c = engine.canvas();
        scene.update(id, |it| it.fit = fit_for(c.width, c.height));
        let clock = engine.audio.clock;
        engine.audio.add(AudioSource::desktop(clock, None, "Desktop Audio"));
        engine.audio.add(AudioSource::microphone(clock, None, "Microphone"));
    }
}

pub fn create_source(engine: &EngineShared, o: &Value) -> Result<Arc<dyn VideoSource>> {
    let name = o["name"].as_str();
    match o["kind"].as_str() {
        Some("scene") => return Ok(Arc::new(SceneSource::new(None, name.or(o["scene"].as_str())))),
        Some("pattern") => {
            let (w, h) = (o["width"].as_u64().unwrap_or(1920) as u32, o["height"].as_u64().unwrap_or(1080) as u32);
            return Ok(Arc::new(TestPatternSource::new(&engine.gpu, w, h, name.unwrap_or("Test pattern"))?));
        }
        _ => {}
    }
    let cursor = o["cursor"].as_bool().unwrap_or(true);
    let exe = o["exe"].as_str();
    let gpu = engine.gpu.clone();
    let src = match o["mode"].as_str() {
        Some("Display") => {
            let mons = enumerate_monitors();
            if mons.is_empty() {
                return Err(anyhow!("no monitor"));
            }
            let idx = (o["monitor"].as_i64().unwrap_or(0).max(0) as usize).min(mons.len() - 1);
            WgcSource::monitor(gpu, &mons[idx], cursor, name)
        }
        Some("Window") => {
            let title = o["title"].as_str();
            let w = enumerate_windows().into_iter().find(|x| Some(x.exe_name.as_str()) == exe && Some(x.title.as_str()) == title).or_else(|| exe.and_then(find_window_by_exe));
            match (w, exe) {
                (Some(w), _) => WgcSource::window(gpu, &w, cursor, name),
                // Window gone since last session: follow the program instead, it re-attaches when it opens.
                (None, Some(e)) => WgcSource::game(gpu, e, cursor, name),
                (None, None) => return Err(anyhow!("window not found")),
            }
        }
        _ => WgcSource::game(gpu, exe.ok_or_else(|| anyhow!("game capture without exe"))?, cursor, name),
    };
    if let Some(r) = o["region"].as_array().filter(|r| r.len() == 4) {
        let g = |i: usize| r[i].as_u64().unwrap_or(0) as u32;
        src.set_region(CaptureRegion { x: g(0), y: g(1), width: g(2), height: g(3) });
    }
    Ok(Arc::new(src))
}
