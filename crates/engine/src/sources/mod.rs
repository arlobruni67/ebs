//! Video sources: anything that provides a GPU texture each frame.
pub mod pattern;
pub mod scene_source;
pub mod wgc;

use crate::compositor::Compositor;
use crate::gpu::{Ctx, Gpu};
use crate::util::AtomicF64;
use std::sync::atomic::{AtomicI64, AtomicU32, AtomicU64, Ordering};
use std::sync::Mutex;
use windows::Win32::Graphics::Direct3D11::ID3D11ShaderResourceView;

/// What a source gets when the render loop asks it to draw its own frame.
pub struct RenderArgs<'a> {
    pub gpu: &'a Gpu,
    pub ctx: &'a Ctx,
    pub compositor: &'a Compositor,
    pub tick: i64,
    pub seconds: f64,
}

/// State every source shares (thread-safe counters read by the UI and the stats).
pub struct SourceBase {
    pub id: u64,
    pub name: Mutex<String>,
    pub status: Mutex<String>,
    pub width: AtomicU32,
    pub height: AtomicU32,
    pub frames_delivered: AtomicU64,
    /// QPC time of the newest frame (capture time for WGC).
    pub last_frame_qpc: AtomicI64,
    /// Smoothed delay between a frame being captured and the render loop using it (ms).
    pub frame_age_ms: AtomicF64,
}

impl SourceBase {
    pub fn new(name: &str) -> Self {
        Self {
            id: crate::scene::next_id(),
            name: Mutex::new(name.to_string()),
            status: Mutex::new("Idle".into()),
            width: AtomicU32::new(0),
            height: AtomicU32::new(0),
            frames_delivered: AtomicU64::new(0),
            last_frame_qpc: AtomicI64::new(0),
            frame_age_ms: AtomicF64::default(),
        }
    }
    pub fn set_status(&self, s: impl Into<String>) {
        *self.status.lock().unwrap() = s.into();
    }
    pub fn set_size(&self, w: u32, h: u32) {
        self.width.store(w, Ordering::Relaxed);
        self.height.store(h, Ordering::Relaxed);
    }
}

pub trait VideoSource: Send + Sync {
    fn base(&self) -> &SourceBase;
    /// Kind shown in the UI ("Display Capture", "Game Capture"...).
    fn kind(&self) -> &'static str;
    /// Current frame, or None before the first frame. Context lock must be held.
    fn frame(&self) -> Option<ID3D11ShaderResourceView>;
    /// Capture sources have no meaningful alpha (WGC gives garbage alpha for some windows).
    fn force_opaque(&self) -> bool {
        true
    }
    /// Once a second on the render thread (reacquire a game window, etc).
    fn tick(&self) {}
    /// True if the render loop should call `render` every frame (generated content, nested scenes).
    fn renders_own_frames(&self) -> bool {
        false
    }
    fn render(&self, _args: &RenderArgs) {}
    /// Settings to save in the scene collection.
    fn save(&self) -> serde_json::Value;
    /// Stops capture and frees GPU resources.
    fn shutdown(&self) {}
    /// Process whose window this source captures (window/game capture), for measuring recording impact.
    fn process_id(&self) -> Option<u32> {
        None
    }

    fn id(&self) -> u64 {
        self.base().id
    }
    fn name(&self) -> String {
        self.base().name.lock().unwrap().clone()
    }
    fn status(&self) -> String {
        self.base().status.lock().unwrap().clone()
    }
    fn size(&self) -> (u32, u32) {
        let b = self.base();
        (b.width.load(Ordering::Relaxed), b.height.load(Ordering::Relaxed))
    }
    fn frames_delivered(&self) -> u64 {
        self.base().frames_delivered.load(Ordering::Relaxed)
    }
    fn as_scene_source(&self) -> Option<&scene_source::SceneSource> {
        None
    }
}
