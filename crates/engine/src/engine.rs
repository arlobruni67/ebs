//! The capture engine: owns the GPU device, sources, the active scene, the audio mixer and a render
//! thread that composites the scene on a fixed clock (constant frame rate, like OBS) and hands each frame
//! to the preview and to outputs (recording, replay buffer). Independent of any UI.
use crate::audio::AudioMixer;
use crate::compositor::Compositor;
use crate::gpu::{Ctx, Gpu};
use crate::scene::Scene;
use crate::sources::{RenderArgs, VideoSource};
use crate::util::{qpc, qpc_freq, AtomicF64, MmcssGuard, PreciseTimer, SendCell, TimerResolution};
use anyhow::Result;
use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use windows::Win32::Foundation::HWND;
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::Common::*;
use windows::Win32::Graphics::Dxgi::*;

/// Receives every composited canvas frame on the render thread (context lock held).
pub trait FrameSink: Send + Sync {
    /// `tick` advances by exactly one per output frame; `tick_qpc` is the time this frame represents.
    fn on_frame(&self, ctx: &Ctx, canvas: &ID3D11Texture2D, canvas_srv: &ID3D11ShaderResourceView, tick: i64, tick_qpc: i64);
}

/// How the picture changes when switching scenes. Values match the transition shader.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum TransitionKind {
    Cut = 0,
    #[default]
    Fade = 1,
    FadeThroughBlack = 2,
    Slide = 3,
    Wipe = 4,
}
impl TransitionKind {
    pub const ALL: [TransitionKind; 5] = [Self::Cut, Self::Fade, Self::FadeThroughBlack, Self::Slide, Self::Wipe];
    pub fn label(self) -> &'static str {
        match self {
            Self::Cut => "Cut",
            Self::Fade => "Fade",
            Self::FadeThroughBlack => "Fade to black",
            Self::Slide => "Slide",
            Self::Wipe => "Wipe",
        }
    }
    pub fn parse(s: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|k| format!("{k:?}").eq_ignore_ascii_case(s))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct VideoCanvas {
    pub width: u32,
    pub height: u32,
    pub fps_num: u32,
    pub fps_den: u32,
}
impl VideoCanvas {
    pub fn new(width: u32, height: u32, fps_num: u32, fps_den: u32) -> Self {
        Self { width, height, fps_num, fps_den: fps_den.max(1) }
    }
    pub fn fps(&self) -> f64 {
        self.fps_num as f64 / self.fps_den as f64
    }
}
impl Default for VideoCanvas {
    fn default() -> Self {
        Self::new(1920, 1080, 60, 1)
    }
}

struct Target {
    tex: ID3D11Texture2D,
    rtv: ID3D11RenderTargetView,
    srv: ID3D11ShaderResourceView,
}
impl Target {
    fn new(gpu: &Gpu, w: u32, h: u32) -> Result<Self> {
        let tex = gpu.texture(w, h, DXGI_FORMAT_B8G8R8A8_UNORM, D3D11_BIND_FLAG(D3D11_BIND_RENDER_TARGET.0 | D3D11_BIND_SHADER_RESOURCE.0))?;
        Ok(Self { rtv: gpu.rtv(&tex, None)?, srv: gpu.srv(&tex)?, tex })
    }
}

struct Transition {
    from: Arc<Scene>,
    kind: TransitionKind,
    seconds: f64,
    start_tick: i64,
}

struct SceneState {
    current: Arc<Scene>,
    transition: Option<Transition>,
}

struct Preview {
    swap: IDXGISwapChain1,
    rtv: Option<ID3D11RenderTargetView>,
    w: u32,
    h: u32,
    resize: Option<(u32, u32)>,
}

/// Render-thread counters (thread-safe reads).
#[derive(Default)]
pub struct EngineStats {
    pub frames_rendered: AtomicU64,
    pub frames_lagged: AtomicU64,
    render_ticks_total: AtomicI64,
    render_ticks_max: AtomicI64,
}
impl EngineStats {
    pub fn average_render_ms(&self) -> f64 {
        let n = self.frames_rendered.load(Ordering::Relaxed);
        if n == 0 {
            0.0
        } else {
            crate::util::qpc_to_ms(self.render_ticks_total.load(Ordering::Relaxed)) / n as f64
        }
    }
    pub fn max_render_ms(&self) -> f64 {
        crate::util::qpc_to_ms(self.render_ticks_max.load(Ordering::Relaxed))
    }
    pub fn reset_peaks(&self) {
        self.render_ticks_max.store(0, Ordering::Relaxed);
    }
    fn add_rendered(&self, ticks: i64) {
        self.frames_rendered.fetch_add(1, Ordering::Relaxed);
        self.render_ticks_total.fetch_add(ticks, Ordering::Relaxed);
        self.render_ticks_max.fetch_max(ticks, Ordering::Relaxed);
    }
}

pub struct EngineShared {
    pub gpu: Arc<Gpu>,
    pub compositor: Compositor,
    pub audio: Arc<AudioMixer>,
    pub stats: EngineStats,
    canvas_settings: Mutex<VideoCanvas>,
    pending_canvas: Mutex<Option<VideoCanvas>>,
    canvas: Mutex<Option<SendCell<Target>>>,
    trans_targets: Mutex<Option<SendCell<(Target, Target)>>>,
    sources: Mutex<Vec<Arc<dyn VideoSource>>>,
    scenes: Mutex<SceneState>,
    sinks: Mutex<Vec<Arc<dyn FrameSink>>>,
    preview: Mutex<Option<SendCell<Preview>>>,
    pub preview_divisor: AtomicU32,
    pub preview_enabled: AtomicBool,
    /// Item outlined in the preview only (never in recordings).
    pub preview_selection: Mutex<Option<u64>>,
    current_tick: AtomicI64,
    epoch_qpc: AtomicI64,
    running: AtomicBool,
    log: Mutex<Vec<String>>,
    canvas_generation: AtomicU64,
    latency_ms: AtomicF64,
}

pub struct Engine {
    pub shared: Arc<EngineShared>,
    thread: Option<JoinHandle<()>>,
}

impl std::ops::Deref for Engine {
    type Target = EngineShared;
    fn deref(&self) -> &EngineShared {
        &self.shared
    }
}

impl Engine {
    pub fn new(canvas: VideoCanvas, adapter: Option<u32>) -> Result<Engine> {
        crate::ff::init()?;
        let adapters = Gpu::enumerate_adapters()?;
        let info = Gpu::choose_adapter(&adapters, adapter).ok_or_else(|| anyhow::anyhow!("no GPU adapter"))?;
        let gpu = Arc::new(Gpu::create(&info)?);
        let compositor = Compositor::new(&gpu)?;
        let target = Target::new(&gpu, canvas.width, canvas.height)?;
        let shared = Arc::new(EngineShared {
            audio: Arc::new(AudioMixer::new()),
            compositor,
            gpu,
            stats: EngineStats::default(),
            canvas_settings: Mutex::new(canvas),
            pending_canvas: Mutex::new(None),
            canvas: Mutex::new(Some(SendCell(target))),
            trans_targets: Mutex::new(None),
            sources: Mutex::new(Vec::new()),
            scenes: Mutex::new(SceneState { current: Scene::new("Scene"), transition: None }),
            sinks: Mutex::new(Vec::new()),
            preview: Mutex::new(None),
            preview_divisor: AtomicU32::new(2),
            preview_enabled: AtomicBool::new(true),
            preview_selection: Mutex::new(None),
            current_tick: AtomicI64::new(0),
            epoch_qpc: AtomicI64::new(qpc()),
            running: AtomicBool::new(true),
            log: Mutex::new(Vec::new()),
            canvas_generation: AtomicU64::new(0),
            latency_ms: AtomicF64::default(),
        });
        shared.audio.start();
        let s2 = shared.clone();
        let thread = std::thread::Builder::new().name("EBS render".into()).spawn(move || s2.render_loop())?;
        Ok(Engine { shared, thread: Some(thread) })
    }
}

impl Drop for Engine {
    fn drop(&mut self) {
        self.shared.running.store(false, Ordering::SeqCst);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
        for s in self.shared.sources.lock().unwrap().drain(..) {
            s.shutdown();
        }
        self.shared.audio.shutdown();
        let _ctx = self.shared.gpu.lock();
        *self.shared.preview.lock().unwrap() = None;
    }
}

impl EngineShared {
    pub fn canvas(&self) -> VideoCanvas {
        *self.canvas_settings.lock().unwrap()
    }
    pub fn current_tick(&self) -> i64 {
        self.current_tick.load(Ordering::Relaxed)
    }
    pub fn epoch_qpc(&self) -> i64 {
        self.epoch_qpc.load(Ordering::Relaxed)
    }
    /// Changes every time the canvas texture is recreated (size or fps change).
    pub fn canvas_generation(&self) -> u64 {
        self.canvas_generation.load(Ordering::Relaxed)
    }

    pub fn log(&self, msg: impl Into<String>) {
        let m = msg.into();
        let mut l = self.log.lock().unwrap();
        if l.len() > 500 {
            l.drain(..250);
        }
        l.push(m);
    }
    /// Takes the log lines written since the last call.
    pub fn drain_log(&self) -> Vec<String> {
        std::mem::take(&mut *self.log.lock().unwrap())
    }

    /// Changes canvas size / fps. Refused while an output is active.
    pub fn set_canvas(&self, c: VideoCanvas) -> Result<()> {
        if !self.sinks.lock().unwrap().is_empty() {
            anyhow::bail!("Stop recording before changing the canvas.");
        }
        if c != self.canvas() {
            *self.pending_canvas.lock().unwrap() = Some(c);
        }
        Ok(())
    }

    pub fn current_scene(&self) -> Arc<Scene> {
        self.scenes.lock().unwrap().current.clone()
    }
    /// Switches instantly.
    pub fn set_current_scene(&self, s: Arc<Scene>) {
        let mut st = self.scenes.lock().unwrap();
        st.transition = None;
        st.current = s;
    }
    /// Switches with a transition (recordings include it). A new switch mid-transition continues from the scene being switched to.
    pub fn switch_scene(&self, to: Arc<Scene>, kind: TransitionKind, ms: u32) {
        let mut st = self.scenes.lock().unwrap();
        if Arc::ptr_eq(&to, &st.current) && st.transition.is_none() {
            return;
        }
        if kind == TransitionKind::Cut || ms == 0 {
            st.transition = None;
        } else {
            st.transition = Some(Transition { from: st.current.clone(), kind, seconds: ms as f64 / 1000.0, start_tick: -1 });
        }
        st.current = to;
    }
    pub fn in_transition(&self) -> bool {
        self.scenes.lock().unwrap().transition.is_some()
    }

    pub fn add_source(&self, s: Arc<dyn VideoSource>) {
        self.sources.lock().unwrap().push(s);
    }
    pub fn sources(&self) -> Vec<Arc<dyn VideoSource>> {
        self.sources.lock().unwrap().clone()
    }
    /// Removes a source (callers remove it from scenes first) and stops it.
    pub fn remove_source(&self, id: u64) {
        let removed: Vec<_> = {
            let mut v = self.sources.lock().unwrap();
            let (gone, keep): (Vec<_>, Vec<_>) = v.drain(..).partition(|s| s.id() == id);
            *v = keep;
            gone
        };
        self.current_scene().remove_source(id);
        for s in removed {
            s.shutdown();
        }
    }

    pub fn add_sink(&self, s: Arc<dyn FrameSink>) {
        self.sinks.lock().unwrap().push(s);
    }
    pub fn remove_sink(&self, s: &Arc<dyn FrameSink>) {
        self.sinks.lock().unwrap().retain(|x| !Arc::ptr_eq(x, s));
    }
    pub fn has_sinks(&self) -> bool {
        !self.sinks.lock().unwrap().is_empty()
    }

    /// Saves the current canvas as a BMP.
    pub fn save_canvas_snapshot(&self, path: &std::path::Path) -> Result<()> {
        let (px, w, h) = {
            let ctx = self.gpu.lock();
            let c = self.canvas.lock().unwrap();
            crate::gpu::read_bgra(&self.gpu, &ctx, &c.as_ref().unwrap().tex)?
        };
        crate::gpu::save_bmp(path, &px, w, h)
    }

    // ------------------------------------------------------------ preview

    pub fn attach_preview(&self, hwnd: isize, w: u32, h: u32) -> Result<()> {
        let _ctx = self.gpu.lock();
        let (w, h) = (w.max(1), h.max(1));
        let desc = DXGI_SWAP_CHAIN_DESC1 {
            Width: w,
            Height: h,
            Format: DXGI_FORMAT_B8G8R8A8_UNORM,
            SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
            BufferUsage: DXGI_USAGE_RENDER_TARGET_OUTPUT,
            BufferCount: 2,
            SwapEffect: DXGI_SWAP_EFFECT_FLIP_DISCARD,
            Scaling: DXGI_SCALING_STRETCH,
            AlphaMode: DXGI_ALPHA_MODE_IGNORE,
            ..Default::default()
        };
        let hwnd = HWND(hwnd as *mut _);
        let swap = unsafe { self.gpu.factory.CreateSwapChainForHwnd(&self.gpu.device, hwnd, &desc, None, None) }?;
        unsafe {
            let _ = self.gpu.factory.MakeWindowAssociation(hwnd, DXGI_MWA_NO_ALT_ENTER | DXGI_MWA_NO_WINDOW_CHANGES);
        }
        let bb: ID3D11Texture2D = unsafe { swap.GetBuffer(0) }?;
        let rtv = self.gpu.rtv(&bb, None)?;
        *self.preview.lock().unwrap() = Some(SendCell(Preview { swap, rtv: Some(rtv), w, h, resize: None }));
        Ok(())
    }

    pub fn resize_preview(&self, w: u32, h: u32) {
        if let Some(p) = self.preview.lock().unwrap().as_mut() {
            p.0.resize = Some((w.max(1), h.max(1)));
        }
    }

    pub fn detach_preview(&self) {
        let _ctx = self.gpu.lock();
        *self.preview.lock().unwrap() = None;
    }

    fn present_preview(&self, ctx: &Ctx, canvas: &Target, cs: VideoCanvas, scene: &Scene) {
        if !self.preview_enabled.load(Ordering::Relaxed) {
            return;
        }
        let mut guard = self.preview.lock().unwrap();
        let Some(p) = guard.as_mut() else { return };
        let p = &mut p.0;
        let ok = (|| -> Result<()> {
            if let Some((w, h)) = p.resize.take() {
                p.rtv = None;
                unsafe {
                    ctx.OMSetRenderTargets(None, None);
                    p.swap.ResizeBuffers(2, w, h, DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_SWAP_CHAIN_FLAG(0))?;
                }
                let bb: ID3D11Texture2D = unsafe { p.swap.GetBuffer(0) }?;
                p.rtv = Some(self.gpu.rtv(&bb, None)?);
                p.w = w;
                p.h = h;
            }
            let rtv = p.rtv.as_ref().unwrap();
            self.compositor.blit(ctx, &canvas.srv, cs.width, cs.height, rtv, p.w, p.h, [0.09, 0.09, 0.1, 1.0]);
            if let Some(sel) = *self.preview_selection.lock().unwrap() {
                if let Some(it) = scene.item(sel).filter(|i| i.visible) {
                    self.compositor.draw_selection(ctx, &it, cs.width, cs.height, rtv, p.w, p.h);
                }
            }
            unsafe { p.swap.Present(0, DXGI_PRESENT(0)).ok()? };
            Ok(())
        })();
        if let Err(e) = ok {
            self.log(format!("preview: {e}"));
            *guard = None;
        }
    }

    // ------------------------------------------------------------ render loop

    fn render_loop(self: Arc<Self>) {
        let _res = TimerResolution::high();
        let _mm = MmcssGuard::capture();
        let timer = PreciseTimer::new();
        let freq = qpc_freq();
        let mut last_housekeeping = 0i64;
        self.epoch_qpc.store(qpc(), Ordering::Relaxed);
        let mut tick: i64 = 0;
        let mut period = freq as f64 / self.canvas().fps();
        while self.running.load(Ordering::Relaxed) {
            let pending = self.pending_canvas.lock().unwrap().take();
            if let Some(pc) = pending {
                let _ctx = self.gpu.lock();
                match Target::new(&self.gpu, pc.width, pc.height) {
                    Ok(t) => {
                        *self.canvas.lock().unwrap() = Some(SendCell(t));
                        *self.trans_targets.lock().unwrap() = None;
                        *self.canvas_settings.lock().unwrap() = pc;
                        self.canvas_generation.fetch_add(1, Ordering::Relaxed);
                        period = freq as f64 / pc.fps();
                        self.epoch_qpc.store(qpc(), Ordering::Relaxed);
                        tick = 0;
                    }
                    Err(e) => self.log(format!("canvas: {e}")),
                }
            }

            let epoch = self.epoch_qpc.load(Ordering::Relaxed);
            let mut target = epoch + (tick as f64 * period) as i64;
            timer.wait_until(target);
            let start = qpc();
            // Fell behind by more than a frame (system stall): skip the missed ticks, count them as render lag.
            let behind = ((start - target) as f64 / period) as i64;
            if behind >= 1 {
                self.stats.frames_lagged.fetch_add(behind as u64, Ordering::Relaxed);
                tick += behind;
                target = epoch + (tick as f64 * period) as i64;
            }
            self.render_frame(tick, target, start, period, &mut last_housekeeping);
            self.current_tick.store(tick, Ordering::Relaxed);
            self.stats.add_rendered(qpc() - start);
            tick += 1;
        }
    }

    fn render_frame(&self, tick: i64, target: i64, start: i64, period: f64, last_housekeeping: &mut i64) {
        let freq = qpc_freq();
        let cs = self.canvas();
        let ctx_guard = self.gpu.lock();
        let ctx: &Ctx = &ctx_guard;
        let sources = self.sources();
        if start - *last_housekeeping > freq {
            *last_housekeeping = start;
            for s in &sources {
                s.tick();
            }
        }

        let (scene, trans) = {
            let mut st = self.scenes.lock().unwrap();
            let mut info = None;
            if let Some(tr) = st.transition.as_mut() {
                if tr.start_tick < 0 {
                    tr.start_tick = tick;
                }
                let t = (tick - tr.start_tick) as f64 / cs.fps() / tr.seconds;
                if t >= 1.0 {
                    st.transition = None;
                } else {
                    info = Some((tr.from.clone(), tr.kind, t.clamp(0.0, 1.0)));
                }
            }
            (st.current.clone(), info)
        };

        let args = RenderArgs { gpu: &self.gpu, ctx, compositor: &self.compositor, tick, seconds: tick as f64 * period / freq as f64 };
        for s in &sources {
            if s.renders_own_frames() && s.as_scene_source().is_none() {
                s.render(&args);
            }
        }
        // Nested scenes: only the ones on screen, deepest first so a parent shows this frame's content.
        let mut nested: Vec<Arc<dyn VideoSource>> = Vec::new();
        let mut seen = HashSet::new();
        collect_nested(&scene, &mut nested, &mut seen, 0);
        if let Some((from, _, _)) = &trans {
            collect_nested(from, &mut nested, &mut seen, 0);
        }
        nested.sort_by_key(|n| std::cmp::Reverse(n.as_scene_source().map(|s| s.depth(0)).unwrap_or(0)));
        for n in &nested {
            if let Some(ss) = n.as_scene_source() {
                ss.render_with(&args, cs.width, cs.height);
            }
        }

        let canvas_guard = self.canvas.lock().unwrap();
        let Some(canvas) = canvas_guard.as_ref() else { return };
        scene.apply_auto_fit(cs.width, cs.height);
        match &trans {
            None => self.compositor.render_items(ctx, &scene.snapshot(), &canvas.rtv, cs.width, cs.height, false),
            Some((from, kind, t)) => {
                from.apply_auto_fit(cs.width, cs.height);
                let mut tt = self.trans_targets.lock().unwrap();
                if tt.is_none() {
                    match (Target::new(&self.gpu, cs.width, cs.height), Target::new(&self.gpu, cs.width, cs.height)) {
                        (Ok(a), Ok(b)) => *tt = Some(SendCell((a, b))),
                        _ => {
                            self.log("transition targets: out of GPU memory");
                        }
                    }
                }
                if let Some(pair) = tt.as_ref() {
                    let (a, b) = &pair.0;
                    self.compositor.render_items(ctx, &from.snapshot(), &a.rtv, cs.width, cs.height, false);
                    self.compositor.render_items(ctx, &scene.snapshot(), &b.rtv, cs.width, cs.height, false);
                    let eased = (t * t * (3.0 - 2.0 * t)) as f32; // smoothstep: gentle start and end
                    self.compositor.render_transition(ctx, &a.srv, &b.srv, &canvas.rtv, cs.width, cs.height, *kind as i32, eased);
                } else {
                    self.compositor.render_items(ctx, &scene.snapshot(), &canvas.rtv, cs.width, cs.height, false);
                }
            }
        }
        self.measure_frame_age(&scene, start);

        let sinks = self.sinks.lock().unwrap().clone();
        for s in &sinks {
            s.on_frame(ctx, &canvas.tex, &canvas.srv, tick, target);
        }
        if tick % self.preview_divisor.load(Ordering::Relaxed).max(1) as i64 == 0 {
            self.present_preview(ctx, canvas, cs, &scene);
        }
        unsafe { ctx.Flush() };
    }

    fn measure_frame_age(&self, scene: &Scene, now: i64) {
        let mut best: Option<(f32, f64)> = None;
        for item in scene.snapshot() {
            let b = item.source.base();
            let f = b.last_frame_qpc.load(Ordering::Relaxed);
            if f == 0 {
                continue;
            }
            let age = crate::util::qpc_to_ms(now - f);
            if !(0.0..=250.0).contains(&age) {
                continue; // static content: no new frames, nothing to measure
            }
            let prev = b.frame_age_ms.load();
            let v = if prev == 0.0 { age } else { prev * 0.98 + age * 0.02 };
            b.frame_age_ms.store(v);
            let area = item.display_width() * item.display_height();
            if item.visible && best.is_none_or(|(a, _)| area > a) {
                best = Some((area, v));
            }
        }
        self.latency_ms.store(best.map(|b| b.1).unwrap_or(0.0));
    }

    /// Delay between something happening on screen and it reaching the encoder (frame age of the largest
    /// visible source). Recordings delay audio by this much so sound lines up with the picture.
    pub fn video_latency_ms(&self) -> f64 {
        self.latency_ms.load()
    }
}

fn collect_nested(scene: &Arc<Scene>, into: &mut Vec<Arc<dyn VideoSource>>, seen: &mut HashSet<u64>, guard: u32) {
    if guard > 16 {
        return;
    }
    for it in scene.snapshot() {
        if !it.visible {
            continue;
        }
        if let Some(ss) = it.source.as_scene_source() {
            if seen.insert(it.source.id()) {
                into.push(it.source.clone());
                if let Some(inner) = ss.inner() {
                    collect_nested(&inner, into, seen, guard + 1);
                }
            }
        }
    }
}
