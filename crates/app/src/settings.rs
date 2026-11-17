//! Everything the user configures, persisted as JSON in the data folder (settings.json).
use ebs_engine::benchmark::{self, PresetTestResult};
use ebs_engine::encoding::{self, QualityPreset, VideoEncoderSettings};
use ebs_engine::gpu::Gpu;
use ebs_engine::muxer::ContainerFormat;
use ebs_engine::probe::ProbeResult;
use ebs_engine::recorder::RecordingSettings;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

pub fn data_dir() -> &'static Path {
    static DIR: OnceLock<PathBuf> = OnceLock::new();
    DIR.get_or_init(|| {
        if let Some(d) = std::env::var_os("EBS_DATADIR").filter(|d| !d.is_empty()) {
            return PathBuf::from(d);
        }
        let appdata = std::env::var_os("APPDATA").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("."));
        appdata.join("EBS-rs")
    })
}

pub fn scenes_dir() -> PathBuf {
    data_dir().join("scenes")
}

pub fn log(line: impl AsRef<str>) {
    use std::io::Write;
    let _ = std::fs::create_dir_all(data_dir());
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(data_dir().join("ebs.log")) {
        let t = unsafe { windows::Win32::System::SystemInformation::GetLocalTime() };
        let _ = writeln!(
            f,
            "{:04}-{:02}-{:02} {:02}:{:02}:{:02}.{:03} {}",
            t.wYear, t.wMonth, t.wDay, t.wHour, t.wMinute, t.wSecond, t.wMilliseconds, line.as_ref()
        );
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VideoMode {
    YouTubeWide,
    YouTubeShort,
    Custom,
}
impl VideoMode {
    pub const ALL: [VideoMode; 3] = [Self::YouTubeWide, Self::YouTubeShort, Self::Custom];
    pub fn label(self) -> &'static str {
        match self {
            Self::YouTubeWide => "YouTube Wide video (16:9)",
            Self::YouTubeShort => "YouTube Short (9:16 vertical)",
            Self::Custom => "Custom resolution",
        }
    }
    fn is_wide(w: u32, h: u32) -> bool {
        w > h && w as u64 * 9 == h as u64 * 16
    }
    fn is_tall(w: u32, h: u32) -> bool {
        h > w && w as u64 * 16 == h as u64 * 9
    }
    pub fn detect(cw: u32, ch: u32, ow: u32, oh: u32) -> VideoMode {
        if Self::is_wide(cw, ch) && Self::is_wide(ow, oh) {
            Self::YouTubeWide
        } else if Self::is_tall(cw, ch) && Self::is_tall(ow, oh) {
            Self::YouTubeShort
        } else {
            Self::Custom
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct AppSettings {
    pub output_dir: PathBuf,
    pub filename_format: String,
    pub container: ContainerFormat,
    pub remux_to_mp4: bool,
    pub canvas_width: u32,
    pub canvas_height: u32,
    pub output_width: u32,
    pub output_height: u32,
    pub fps_num: u32,
    pub fps_den: u32,
    /// Manual ("Custom") encoder settings, used as-is when `encoder_preset` is None.
    pub video: VideoEncoderSettings,
    pub color_space_601: bool,
    pub color_range_full: bool,
    /// SMOOTH / BALANCED / QUALITY, or None for Custom.
    pub encoder_preset: Option<String>,
    /// True once the user picked a preset themselves; until then the benchmark's recommendation is applied.
    pub preset_chosen_by_user: bool,
    pub preset_results_fingerprint: Option<String>,
    pub preset_results: Vec<PresetTestResult>,
    pub audio_codec: String,
    pub audio_bitrate_kbps: u32,
    pub audio_tracks: u8,
    pub auto_av_sync: bool,
    pub audio_offset_ms: i32,
    pub preview_fps_divisor: u32,
    pub collection: String,
    pub preview_enabled: bool,
    /// Encoder probe results, valid while the GPU/driver fingerprint matches.
    pub probe_fingerprint: Option<String>,
    pub probe: Vec<ProbeResult>,
    /// Set once the C# EBS settings/collections were imported (or there was nothing to import).
    pub imported: bool,
}

impl Default for AppSettings {
    fn default() -> Self {
        Self {
            output_dir: default_output_dir(),
            filename_format: "yyyy-MM-dd HH-mm-ss".into(),
            container: ContainerFormat::Mkv,
            remux_to_mp4: false,
            canvas_width: 1920,
            canvas_height: 1080,
            output_width: 1920,
            output_height: 1080,
            fps_num: 60,
            fps_den: 1,
            video: VideoEncoderSettings::default(),
            color_space_601: false,
            color_range_full: false,
            encoder_preset: Some("BALANCED".into()),
            preset_chosen_by_user: false,
            preset_results_fingerprint: None,
            preset_results: Vec::new(),
            audio_codec: "aac".into(),
            audio_bitrate_kbps: 192,
            audio_tracks: 0b111,
            auto_av_sync: true,
            audio_offset_ms: 0,
            preview_fps_divisor: 2,
            collection: "Default".into(),
            preview_enabled: true,
            probe_fingerprint: None,
            probe: Vec::new(),
            imported: false,
        }
    }
}

/// Recordings go to the fixed drive with the most free space (C: is nearly full on this PC, D: is not).
pub fn default_output_dir() -> PathBuf {
    let hw = ebs_engine::hardware::HardwareInfo::detect();
    let sys = std::env::var("SystemDrive").unwrap_or_else(|_| "C:".into());
    match hw.drives.iter().filter(|d| !d.removable).max_by_key(|d| d.free_gb) {
        Some(d) if !d.name.to_uppercase().starts_with(&sys.to_uppercase()) => PathBuf::from(&d.name).join("EBS Recordings"),
        _ => std::env::var_os("USERPROFILE").map(|p| PathBuf::from(p).join("Videos").join("EBS")).unwrap_or_else(|| PathBuf::from("EBS Recordings")),
    }
}

pub fn hardware_fingerprint() -> String {
    static FP: OnceLock<String> = OnceLock::new();
    FP.get_or_init(|| {
        let ids: Vec<String> = Gpu::enumerate_adapters().unwrap_or_default().iter().map(|a| format!("{:04X}:{:04X}", a.vendor_id, a.device_id)).collect();
        format!("{}|{}|ffmpeg63-rs", ids.join("|"), ebs_engine::nvml::driver_version().unwrap_or_default())
    })
    .clone()
}

impl AppSettings {
    pub fn path() -> PathBuf {
        data_dir().join("settings.json")
    }

    pub fn load() -> AppSettings {
        let p = Self::path();
        match std::fs::read_to_string(&p).ok().map(|t| serde_json::from_str::<AppSettings>(&t)) {
            Some(Ok(mut s)) => {
                if s.filename_format.trim().is_empty() {
                    s.filename_format = "yyyy-MM-dd HH-mm-ss".into();
                }
                s
            }
            Some(Err(e)) => {
                log(format!("settings load failed: {e}"));
                AppSettings::default()
            }
            None => AppSettings::default(),
        }
    }

    pub fn save(&self) {
        let _ = std::fs::create_dir_all(data_dir());
        let p = Self::path();
        match serde_json::to_string_pretty(self) {
            Ok(t) => {
                if let Err(e) = ebs_engine::util::write_replace(&p, t.as_bytes()) {
                    log(format!("settings save failed: {e}"));
                }
            }
            Err(e) => log(format!("settings save failed: {e}")),
        }
    }

    /// One-time import from the C# EBS (%APPDATA%\ArloCapture): settings and scene collections are copied,
    /// never changed in place. Preset test results are not imported: presets must be re-tested with this
    /// recorder before they count as tested.
    pub fn import_from_csharp(&mut self) {
        if self.imported {
            return;
        }
        self.imported = true;
        let Some(appdata) = std::env::var_os("APPDATA").map(PathBuf::from) else { return };
        let old = appdata.join("ArloCapture");
        if let Ok(t) = std::fs::read_to_string(old.join("settings.json")) {
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(&t) {
                let s = |k: &str| v[k].as_str().map(String::from);
                let u = |k: &str| v[k].as_u64().map(|x| x as u32);
                let b = |k: &str| v[k].as_bool();
                if let Some(d) = s("OutputDirectory") {
                    self.output_dir = d.into();
                }
                if let Some(f) = s("FilenameFormat").filter(|f| !f.trim().is_empty()) {
                    self.filename_format = f;
                }
                self.container = match s("Container").as_deref() {
                    Some("Mp4") => ContainerFormat::Mp4,
                    Some("Mov") => ContainerFormat::Mov,
                    _ => ContainerFormat::Mkv,
                };
                self.remux_to_mp4 = b("RemuxToMp4").unwrap_or(false);
                self.canvas_width = u("CanvasWidth").unwrap_or(1920);
                self.canvas_height = u("CanvasHeight").unwrap_or(1080);
                self.output_width = u("OutputWidth").unwrap_or(1920);
                self.output_height = u("OutputHeight").unwrap_or(1080);
                self.fps_num = u("FpsNum").unwrap_or(60);
                self.fps_den = u("FpsDen").unwrap_or(1);
                if let Some(id) = v["Video"]["EncoderId"].as_str() {
                    self.video.encoder_id = id.into();
                }
                self.encoder_preset = s("EncoderPreset").map(|p| p.to_uppercase());
                self.audio_codec = s("AudioCodec").unwrap_or_else(|| "aac".into());
                self.audio_bitrate_kbps = u("AudioBitrateKbps").unwrap_or(192);
                self.audio_tracks = u("AudioTracks").unwrap_or(0b111) as u8;
                self.auto_av_sync = b("AutoAvSync").unwrap_or(true);
                self.audio_offset_ms = v["AudioOffsetMs"].as_i64().unwrap_or(0) as i32;
                self.preview_fps_divisor = u("PreviewFpsDivisor").unwrap_or(2).max(1);
                self.collection = s("Collection").unwrap_or_else(|| "Default".into());
                self.preview_enabled = b("PreviewEnabled").unwrap_or(true);
                log("imported settings from the C# EBS");
            }
        }
        let dst = scenes_dir();
        if let Ok(rd) = std::fs::read_dir(old.join("scenes")) {
            let _ = std::fs::create_dir_all(&dst);
            for e in rd.flatten() {
                let p = e.path();
                if p.extension().is_some_and(|x| x == "json") {
                    let to = dst.join(p.file_name().unwrap());
                    if !to.exists() && std::fs::copy(&p, &to).is_ok() {
                        log(format!("imported collection {}", p.display()));
                    }
                }
            }
        }
    }

    pub fn preset(&self) -> Option<QualityPreset> {
        self.encoder_preset.as_deref().and_then(QualityPreset::parse)
    }

    pub fn verified_features(&self, id: &str) -> u32 {
        self.probe.iter().find(|p| p.encoder_id == id).map(|p| p.verified_features).unwrap_or(0)
    }

    pub fn working_encoders(&self) -> Vec<&'static encoding::EncoderInfo> {
        encoding::CATALOG.iter().filter(|e| self.probe.iter().any(|p| p.encoder_id == e.id && p.works)).collect()
    }

    pub fn probe_valid(&self) -> bool {
        !self.probe.is_empty() && self.probe_fingerprint.as_deref() == Some(hardware_fingerprint().as_str())
    }

    fn color(&self) -> (ebs_engine::compositor::ColorSpace, ebs_engine::compositor::ColorRange) {
        use ebs_engine::compositor::{ColorRange, ColorSpace};
        (
            if self.color_space_601 { ColorSpace::Rec601 } else { ColorSpace::Rec709 },
            if self.color_range_full { ColorRange::Full } else { ColorRange::Limited },
        )
    }

    /// The encoder settings a recording will use: the preset's (built from verified features) or the manual ones.
    pub fn effective_video(&self) -> VideoEncoderSettings {
        self.effective_video_for(&self.video.encoder_id, self.preset())
    }

    pub fn effective_video_for(&self, encoder_id: &str, preset: Option<QualityPreset>) -> VideoEncoderSettings {
        let mut v = VideoEncoderSettings {
            encoder_id: encoder_id.into(),
            width: self.output_width,
            height: self.output_height,
            fps_num: self.fps_num,
            fps_den: self.fps_den,
            ..self.video.clone()
        };
        let (cs, cr) = self.color();
        v.color_space = cs;
        v.color_range = cr;
        if let Some(p) = preset {
            v = p.build(encoder_id, self.verified_features(encoder_id), &v);
            v.color_space = cs;
            v.color_range = cr;
        }
        v
    }

    pub fn to_recording_settings(&self) -> RecordingSettings {
        RecordingSettings {
            output_dir: self.output_dir.clone(),
            filename_format: self.filename_format.clone(),
            container: self.container,
            remux_to_mp4: self.remux_to_mp4 && self.container == ContainerFormat::Mkv,
            video: self.effective_video(),
            audio_codec: self.audio_codec.clone(),
            audio_bitrate_kbps: self.audio_bitrate_kbps,
            tracks: self.audio_tracks,
            auto_av_sync: self.auto_av_sync,
            av_offset_ms: self.audio_offset_ms as f64,
            ..RecordingSettings::default()
        }
    }

    /// The benchmark result of a preset for the current hardware, encoder, output size and frame rate.
    pub fn result_for(&self, p: QualityPreset, encoder_id: Option<&str>) -> Option<&PresetTestResult> {
        if self.preset_results_fingerprint.as_deref() != Some(hardware_fingerprint().as_str()) {
            return None;
        }
        let id = encoder_id.unwrap_or(&self.video.encoder_id);
        self.preset_results
            .iter()
            .rev()
            .find(|r| r.preset() == Some(p) && r.matches(id, self.output_width, self.output_height, self.fps_num, self.fps_den))
    }

    /// Recommended preset once all three were tested for this setup, else None.
    pub fn recommended_preset(&self, encoder_id: Option<&str>) -> Option<QualityPreset> {
        let rs: Vec<PresetTestResult> = QualityPreset::ALL.iter().filter_map(|p| self.result_for(*p, encoder_id).cloned()).collect();
        (rs.len() == QualityPreset::ALL.len()).then(|| benchmark::recommend(&rs))
    }

    pub fn store_preset_results(&mut self, results: &[PresetTestResult]) {
        let fp = hardware_fingerprint();
        if self.preset_results_fingerprint.as_deref() != Some(fp.as_str()) {
            self.preset_results.clear();
        }
        self.preset_results_fingerprint = Some(fp);
        for r in results {
            self.preset_results.retain(|x| !(x.preset == r.preset && x.matches(&r.encoder_id, r.width, r.height, r.fps_num, r.fps_den)));
            self.preset_results.push(r.clone());
        }
    }

    pub fn video_mode(&self) -> VideoMode {
        VideoMode::detect(self.canvas_width, self.canvas_height, self.output_width, self.output_height)
    }

    /// Applies a quick mode. Keeps the current size when it already matches the mode.
    pub fn apply_mode(&mut self, m: VideoMode) {
        if m == VideoMode::Custom || self.video_mode() == m {
            return;
        }
        let (w, h) = if m == VideoMode::YouTubeShort { (1080, 1920) } else { (1920, 1080) };
        self.canvas_width = w;
        self.output_width = w;
        self.canvas_height = h;
        self.output_height = h;
    }
}
