//! Encoder catalog (what each encoder supports), encoder settings and the SMOOTH/BALANCED/QUALITY presets.
use crate::compositor::{ColorRange, ColorSpace};
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum VideoCodec {
    H264,
    Hevc,
    Av1,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Backend {
    Nvenc,
    Amf,
    X264,
    SvtAv1,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum RateControl {
    #[default]
    ConstantQuality,
    Cqp,
    Vbr,
    Cbr,
}
impl RateControl {
    pub const ALL: [RateControl; 4] = [Self::ConstantQuality, Self::Cqp, Self::Vbr, Self::Cbr];
    pub fn label(self) -> &'static str {
        match self {
            Self::ConstantQuality => "Constant quality (recommended)",
            Self::Cqp => "Constant QP",
            Self::Vbr => "Variable bitrate",
            Self::Cbr => "Constant bitrate",
        }
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum Multipass {
    #[default]
    Disabled,
    QuarterRes,
    FullRes,
}

/// Feature bit flags.
pub mod feature {
    pub const B_FRAMES: u32 = 1 << 0;
    pub const B_FRAME_REFS: u32 = 1 << 1;
    pub const LOOK_AHEAD: u32 = 1 << 2;
    pub const SPATIAL_AQ: u32 = 1 << 3;
    pub const TEMPORAL_AQ: u32 = 1 << 4;
    pub const MULTIPASS: u32 = 1 << 5;
    pub const PSYCHO_VISUAL: u32 = 1 << 6;
    pub const TEN_BIT: u32 = 1 << 7;
    pub const TUNE: u32 = 1 << 8;
    pub const PRE_ANALYSIS: u32 = 1 << 9;
    pub const NAMES: [(u32, &str); 10] = [
        (B_FRAMES, "BFrames"),
        (B_FRAME_REFS, "BFrameRefs"),
        (LOOK_AHEAD, "LookAhead"),
        (SPATIAL_AQ, "SpatialAq"),
        (TEMPORAL_AQ, "TemporalAq"),
        (MULTIPASS, "Multipass"),
        (PSYCHO_VISUAL, "PsychoVisual"),
        (TEN_BIT, "TenBit"),
        (TUNE, "Tune"),
        (PRE_ANALYSIS, "PreAnalysis"),
    ];
    pub fn describe(f: u32) -> String {
        NAMES.iter().filter(|(b, _)| f & b != 0).map(|(_, n)| *n).collect::<Vec<_>>().join(", ")
    }
}

#[derive(Debug)]
pub struct EncoderInfo {
    pub id: &'static str,
    pub ff_name: &'static str,
    pub display_name: &'static str,
    pub codec: VideoCodec,
    pub backend: Backend,
    pub hardware: bool,
    pub features: u32,
    pub rate_controls: &'static [RateControl],
    pub presets: &'static [&'static str],
    pub default_preset: &'static str,
    pub profiles: &'static [&'static str],
    pub tunes: &'static [&'static str],
}

impl EncoderInfo {
    pub fn has(&self, f: u32) -> bool {
        self.features & f == f
    }
    pub fn preset_label(&self, p: &str) -> String {
        match (self.backend, p) {
            (Backend::Nvenc, "p1") => "P1: Fastest (lowest quality)".into(),
            (Backend::Nvenc, "p2") => "P2: Faster".into(),
            (Backend::Nvenc, "p3") => "P3: Fast".into(),
            (Backend::Nvenc, "p4") => "P4: Medium".into(),
            (Backend::Nvenc, "p5") => "P5: Slow (good quality)".into(),
            (Backend::Nvenc, "p6") => "P6: Slower (better quality)".into(),
            (Backend::Nvenc, "p7") => "P7: Slowest (best quality)".into(),
            _ => {
                let mut c = p.chars();
                c.next().map(|f| f.to_uppercase().collect::<String>() + c.as_str()).unwrap_or_default()
            }
        }
    }
}

const NV_PRESETS: &[&str] = &["p1", "p2", "p3", "p4", "p5", "p6", "p7"];
const ALL_RC: &[RateControl] = &RateControl::ALL;
use feature::*;

pub static CATALOG: &[EncoderInfo] = &[
    EncoderInfo {
        id: "nvenc_h264", ff_name: "h264_nvenc", display_name: "NVIDIA NVENC H.264", codec: VideoCodec::H264, backend: Backend::Nvenc, hardware: true,
        features: B_FRAMES | B_FRAME_REFS | LOOK_AHEAD | SPATIAL_AQ | TEMPORAL_AQ | MULTIPASS | TUNE,
        rate_controls: ALL_RC, presets: NV_PRESETS, default_preset: "p5", profiles: &["high", "main", "baseline"], tunes: &["hq", "ll", "ull"],
    },
    EncoderInfo {
        id: "nvenc_hevc", ff_name: "hevc_nvenc", display_name: "NVIDIA NVENC HEVC (H.265)", codec: VideoCodec::Hevc, backend: Backend::Nvenc, hardware: true,
        features: B_FRAMES | B_FRAME_REFS | LOOK_AHEAD | SPATIAL_AQ | TEMPORAL_AQ | MULTIPASS | TEN_BIT | TUNE,
        rate_controls: ALL_RC, presets: NV_PRESETS, default_preset: "p5", profiles: &["main", "main10"], tunes: &["hq", "ll", "ull"],
    },
    EncoderInfo {
        id: "nvenc_av1", ff_name: "av1_nvenc", display_name: "NVIDIA NVENC AV1", codec: VideoCodec::Av1, backend: Backend::Nvenc, hardware: true,
        features: B_FRAMES | LOOK_AHEAD | SPATIAL_AQ | TEMPORAL_AQ | MULTIPASS | TEN_BIT | TUNE,
        rate_controls: ALL_RC, presets: NV_PRESETS, default_preset: "p5", profiles: &["main"], tunes: &["hq", "ll", "ull"],
    },
    // Pre-analysis is deliberately absent: on this PC's AMD driver it segfaults the encoder process.
    EncoderInfo {
        id: "amf_h264", ff_name: "h264_amf", display_name: "AMD AMF H.264", codec: VideoCodec::H264, backend: Backend::Amf, hardware: true,
        features: B_FRAMES, rate_controls: ALL_RC, presets: &["speed", "balanced", "quality"], default_preset: "quality", profiles: &["high", "main"], tunes: &[],
    },
    EncoderInfo {
        id: "amf_hevc", ff_name: "hevc_amf", display_name: "AMD AMF HEVC (H.265)", codec: VideoCodec::Hevc, backend: Backend::Amf, hardware: true,
        features: 0, rate_controls: ALL_RC, presets: &["speed", "balanced", "quality"], default_preset: "quality", profiles: &["main"], tunes: &[],
    },
    EncoderInfo {
        id: "amf_av1", ff_name: "av1_amf", display_name: "AMD AMF AV1", codec: VideoCodec::Av1, backend: Backend::Amf, hardware: true,
        features: 0, rate_controls: ALL_RC, presets: &["speed", "balanced", "quality"], default_preset: "quality", profiles: &["main"], tunes: &[],
    },
    EncoderInfo {
        id: "x264", ff_name: "libx264", display_name: "x264 (CPU) H.264", codec: VideoCodec::H264, backend: Backend::X264, hardware: false,
        features: B_FRAMES | LOOK_AHEAD | PSYCHO_VISUAL | TUNE, rate_controls: ALL_RC,
        presets: &["ultrafast", "superfast", "veryfast", "faster", "fast", "medium", "slow"], default_preset: "veryfast",
        profiles: &["high", "main", "baseline"], tunes: &["film", "animation", "grain", "zerolatency"],
    },
    EncoderInfo {
        id: "svtav1", ff_name: "libsvtav1", display_name: "SVT-AV1 (CPU)", codec: VideoCodec::Av1, backend: Backend::SvtAv1, hardware: false,
        features: LOOK_AHEAD, rate_controls: &[RateControl::ConstantQuality, RateControl::Vbr], presets: &["12", "11", "10", "9", "8"], default_preset: "12",
        profiles: &["main"], tunes: &[],
    },
];

pub fn get(id: &str) -> &'static EncoderInfo {
    CATALOG.iter().find(|e| e.id == id).unwrap_or(&CATALOG[1])
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct VideoEncoderSettings {
    pub encoder_id: String,
    pub width: u32,
    pub height: u32,
    pub fps_num: u32,
    pub fps_den: u32,
    pub rate_control: RateControl,
    pub quality: i32,
    pub bitrate_kbps: u32,
    pub max_bitrate_kbps: u32,
    pub keyframe_seconds: f64,
    pub preset: Option<String>,
    pub profile: Option<String>,
    pub tune: Option<String>,
    pub b_frames: i32,
    pub b_frame_refs: bool,
    pub look_ahead: i32,
    pub spatial_aq: bool,
    pub temporal_aq: bool,
    pub aq_strength: i32,
    pub multipass: Multipass,
    pub psycho_visual: bool,
    pub ten_bit: bool,
    #[serde(skip)]
    pub color_space: ColorSpace,
    #[serde(skip)]
    pub color_range: ColorRange,
}

impl Default for VideoEncoderSettings {
    fn default() -> Self {
        Self {
            encoder_id: "nvenc_hevc".into(),
            width: 1920,
            height: 1080,
            fps_num: 60,
            fps_den: 1,
            rate_control: RateControl::ConstantQuality,
            quality: 20,
            bitrate_kbps: 40000,
            max_bitrate_kbps: 60000,
            keyframe_seconds: 2.0,
            preset: None,
            profile: None,
            tune: Some("hq".into()),
            b_frames: 2,
            b_frame_refs: true,
            look_ahead: 0,
            spatial_aq: true,
            temporal_aq: false,
            aq_strength: 8,
            multipass: Multipass::Disabled,
            psycho_visual: true,
            ten_bit: false,
            color_space: ColorSpace::Rec709,
            color_range: ColorRange::Limited,
        }
    }
}
impl VideoEncoderSettings {
    pub fn fps(&self) -> f64 {
        self.fps_num as f64 / self.fps_den.max(1) as f64
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum QualityPreset {
    Smooth,
    Balanced,
    Quality,
}
impl QualityPreset {
    pub const ALL: [QualityPreset; 3] = [Self::Smooth, Self::Balanced, Self::Quality];
    pub fn label(self) -> &'static str {
        match self {
            Self::Smooth => "SMOOTH",
            Self::Balanced => "BALANCED",
            Self::Quality => "QUALITY",
        }
    }
    pub fn describe(self) -> &'static str {
        match self {
            Self::Smooth => "Least work for the encoder. Use if the game or recording ever stutters. Slightly softer detail in fast motion.",
            Self::Balanced => "Good detail with plenty of headroom. The right choice for most gameplay.",
            Self::Quality => "Best detail per megabyte: slower encoder settings, look-ahead and adaptive quantisation. Needs the most encoder headroom.",
        }
    }
    pub fn parse(s: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|p| p.label().eq_ignore_ascii_case(s))
    }

    /// Builds encoder settings for this preset, using only features verified on this PC (`verified`).
    pub fn build(self, encoder_id: &str, verified: u32, current: &VideoEncoderSettings) -> VideoEncoderSettings {
        let info = get(encoder_id);
        let has = |f: u32| info.has(f) && verified & f == f;
        // Bitrate caps scale with pixels per second; 1080p60 is the reference.
        let scale = (current.width as f64 * current.height as f64 * current.fps() / (1920.0 * 1080.0 * 60.0)).max(0.25);
        let cap = |mbps: u32| ((mbps as f64 * scale).round() as u32) * 1000;
        let mut s = VideoEncoderSettings {
            encoder_id: encoder_id.into(),
            rate_control: RateControl::ConstantQuality,
            keyframe_seconds: 2.0,
            multipass: Multipass::Disabled,
            look_ahead: 0,
            temporal_aq: false,
            ten_bit: false,
            ..current.clone()
        };
        let pick = |a: i32, b: i32, c: i32| match self {
            Self::Smooth => a,
            Self::Balanced => b,
            Self::Quality => c,
        };
        match info.backend {
            Backend::Nvenc => match self {
                Self::Smooth => {
                    s.preset = Some("p2".into());
                    s.tune = Some("hq".into());
                    s.quality = 23;
                    s.max_bitrate_kbps = cap(40);
                    s.b_frames = 0;
                    s.b_frame_refs = false;
                    s.spatial_aq = false;
                }
                Self::Balanced => {
                    s.preset = Some("p4".into());
                    s.tune = Some("hq".into());
                    s.quality = 21;
                    s.max_bitrate_kbps = cap(50);
                    s.b_frames = if has(B_FRAMES) { 2 } else { 0 };
                    s.b_frame_refs = has(B_FRAME_REFS);
                    s.spatial_aq = has(SPATIAL_AQ);
                    s.aq_strength = 8;
                }
                Self::Quality => {
                    s.preset = Some("p6".into());
                    s.tune = Some("hq".into());
                    s.quality = 19;
                    s.max_bitrate_kbps = cap(60);
                    s.b_frames = if has(B_FRAMES) { 3 } else { 0 };
                    s.b_frame_refs = has(B_FRAME_REFS);
                    s.spatial_aq = has(SPATIAL_AQ);
                    s.aq_strength = 8;
                    s.temporal_aq = has(TEMPORAL_AQ);
                    s.look_ahead = if has(LOOK_AHEAD) { 16 } else { 0 };
                    s.multipass = if has(MULTIPASS) { Multipass::QuarterRes } else { Multipass::Disabled };
                }
            },
            Backend::Amf => {
                s.preset = Some(["speed", "balanced", "quality"][pick(0, 1, 2) as usize].into());
                s.quality = pick(23, 21, 19);
                s.max_bitrate_kbps = cap(pick(40, 50, 60) as u32);
                s.b_frames = if self != Self::Smooth && has(B_FRAMES) { 2 } else { 0 };
                s.b_frame_refs = false;
                s.spatial_aq = false;
            }
            Backend::X264 => {
                s.preset = Some(["superfast", "veryfast", "faster"][pick(0, 1, 2) as usize].into());
                s.tune = None;
                s.quality = pick(23, 21, 19);
                s.max_bitrate_kbps = cap(pick(40, 50, 60) as u32);
                s.b_frames = if self == Self::Smooth { 0 } else { 2 };
                s.b_frame_refs = false;
                s.spatial_aq = false;
                s.look_ahead = if self == Self::Quality && has(LOOK_AHEAD) { 20 } else { 0 };
                s.psycho_visual = has(PSYCHO_VISUAL);
            }
            Backend::SvtAv1 => {
                s.preset = Some(["12", "11", "10"][pick(0, 1, 2) as usize].into());
                s.tune = None;
                s.quality = pick(34, 32, 30);
                s.max_bitrate_kbps = cap(pick(30, 40, 50) as u32);
                s.b_frames = 0;
                s.b_frame_refs = false;
                s.spatial_aq = false;
            }
        }
        s
    }
}
