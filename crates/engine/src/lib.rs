//! EBS capture engine (Rust port).
pub mod audio;
pub mod audio_encoder;
pub mod benchmark;
pub mod encoding;
pub mod muxer;
pub mod recorder;
pub mod video_encoder;
pub mod capture;
pub mod collection;
pub mod compositor;
pub mod engine;
pub mod ff;
pub mod gpu;
pub mod hardware;
pub mod nvml;
pub mod probe;
pub mod math;
pub mod monitor;
pub mod scene;
pub mod sources;
pub mod util;

pub use engine::{Engine, FrameSink, TransitionKind, VideoCanvas};
