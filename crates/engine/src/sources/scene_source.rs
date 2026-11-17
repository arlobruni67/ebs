//! A scene shown inside another scene. Renders at canvas size with a transparent background.
use super::{RenderArgs, SourceBase, VideoSource};
use crate::scene::Scene;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex};
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_B8G8R8A8_UNORM;

struct Target {
    _tex: ID3D11Texture2D,
    rtv: ID3D11RenderTargetView,
    srv: ID3D11ShaderResourceView,
}

pub struct SceneSource {
    base: SourceBase,
    pub scene: Mutex<Option<Arc<Scene>>>,
    /// Name kept even when the scene is missing (collection loading order).
    pub scene_name: Mutex<String>,
    target: Mutex<Option<Target>>,
    rendered_tick: AtomicI64,
}
unsafe impl Send for SceneSource {}
unsafe impl Sync for SceneSource {}

impl SceneSource {
    pub fn new(scene: Option<Arc<Scene>>, name: Option<&str>) -> Self {
        let sname = scene.as_ref().map(|s| s.name()).or(name.map(String::from)).unwrap_or_default();
        let base = SourceBase::new(name.unwrap_or(&sname));
        base.set_status("Scene");
        Self { base, scene: Mutex::new(scene), scene_name: Mutex::new(sname), target: Mutex::new(None), rendered_tick: AtomicI64::new(-1) }
    }

    pub fn inner(&self) -> Option<Arc<Scene>> {
        self.scene.lock().unwrap().clone()
    }

    /// True if `scene` is `target` or shows it (directly or nested). Absurd nesting counts as a cycle.
    pub fn contains(scene: &Arc<Scene>, target: &Arc<Scene>, depth: u32) -> bool {
        if Arc::ptr_eq(scene, target) || depth > 16 {
            return true;
        }
        scene.snapshot().iter().any(|it| it.source.as_scene_source().and_then(|s| s.inner()).is_some_and(|inner| Self::contains(&inner, target, depth + 1)))
    }

    pub fn depth(&self, guard: u32) -> u32 {
        let Some(s) = self.inner() else { return 0 };
        if guard > 16 {
            return 0;
        }
        s.snapshot().iter().filter_map(|it| it.source.as_scene_source().map(|ss| 1 + ss.depth(guard + 1))).max().unwrap_or(0)
    }

    pub fn render_with(&self, a: &RenderArgs, cw: u32, ch: u32) {
        if self.rendered_tick.swap(a.tick, Ordering::Relaxed) == a.tick {
            return;
        }
        let Some(scene) = self.inner() else { return };
        let mut t = self.target.lock().unwrap();
        if t.is_none() || self.size() != (cw, ch) {
            let Ok(tex) = a.gpu.texture(cw, ch, DXGI_FORMAT_B8G8R8A8_UNORM, D3D11_BIND_FLAG(D3D11_BIND_RENDER_TARGET.0 | D3D11_BIND_SHADER_RESOURCE.0)) else { return };
            let (Ok(rtv), Ok(srv)) = (a.gpu.rtv(&tex, None), a.gpu.srv(&tex)) else { return };
            *t = Some(Target { _tex: tex, rtv, srv });
            self.base.set_size(cw, ch);
        }
        scene.apply_auto_fit(cw, ch);
        a.compositor.render_items(a.ctx, &scene.snapshot(), &t.as_ref().unwrap().rtv, cw, ch, true);
        self.base.frames_delivered.fetch_add(1, Ordering::Relaxed);
    }
}

impl VideoSource for SceneSource {
    fn base(&self) -> &SourceBase {
        &self.base
    }
    fn kind(&self) -> &'static str {
        "Scene"
    }
    fn force_opaque(&self) -> bool {
        false
    }
    fn renders_own_frames(&self) -> bool {
        true
    }
    fn frame(&self) -> Option<ID3D11ShaderResourceView> {
        self.target.lock().unwrap().as_ref().map(|t| t.srv.clone())
    }
    fn save(&self) -> serde_json::Value {
        let name = self.inner().map(|s| s.name()).unwrap_or_else(|| self.scene_name.lock().unwrap().clone());
        serde_json::json!({ "kind": "scene", "scene": name })
    }
    fn shutdown(&self) {
        *self.target.lock().unwrap() = None;
    }
    fn as_scene_source(&self) -> Option<&SceneSource> {
        Some(self)
    }
}
