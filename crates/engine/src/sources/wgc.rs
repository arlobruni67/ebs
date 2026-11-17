//! Windows.Graphics.Capture source: a monitor, a window, or a game found by its exe (re-acquired when the
//! game restarts). No hooks or injection, so it is safe with anti-cheat (Roblox).
use super::{SourceBase, VideoSource};
use crate::capture::{self, MonitorTarget, WindowTarget};
use crate::gpu::Gpu;
use anyhow::{anyhow, Result};
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, Instant};
use windows::core::Interface;
use windows::Foundation::TypedEventHandler;
use windows::Graphics::Capture::{Direct3D11CaptureFramePool, GraphicsCaptureItem, GraphicsCaptureSession};
use windows::Graphics::DirectX::DirectXPixelFormat;
use windows::Graphics::SizeInt32;
use windows::Win32::Foundation::HWND;
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_B8G8R8A8_UNORM;
use windows::Win32::Graphics::Gdi::HMONITOR;
use windows::Win32::System::WinRT::Direct3D11::IDirect3DDxgiInterfaceAccess;
use windows::Win32::System::WinRT::Graphics::Capture::IGraphicsCaptureItemInterop;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CaptureMode {
    Display,
    Window,
    Game,
}

/// Part of the captured image to keep (pixels). Empty = everything.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CaptureRegion {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
}
impl CaptureRegion {
    pub fn is_empty(&self) -> bool {
        self.width == 0 || self.height == 0
    }
}

struct Session {
    item: GraphicsCaptureItem,
    pool: Direct3D11CaptureFramePool,
    session: GraphicsCaptureSession,
    pool_size: SizeInt32,
}

struct Copy {
    tex: ID3D11Texture2D,
    srv: ID3D11ShaderResourceView,
}

struct Inner {
    base: SourceBase,
    gpu: Arc<Gpu>,
    mode: CaptureMode,
    hwnd: Mutex<isize>,
    hmonitor: isize,
    exe: Option<String>,
    monitor_index: i32,
    cursor: bool,
    region: Mutex<CaptureRegion>,
    session: Mutex<Option<Session>>,
    copy: Mutex<Option<Copy>>,
    next_reacquire: Mutex<Instant>,
}
unsafe impl Send for Inner {}
unsafe impl Sync for Inner {}

pub struct WgcSource(Arc<Inner>);

impl WgcSource {
    fn make(gpu: Arc<Gpu>, name: &str, mode: CaptureMode, hwnd: isize, hmon: isize, exe: Option<String>, monitor_index: i32, cursor: bool) -> Self {
        WgcSource(Arc::new(Inner {
            base: SourceBase::new(name),
            gpu,
            mode,
            hwnd: Mutex::new(hwnd),
            hmonitor: hmon,
            exe,
            monitor_index,
            cursor,
            region: Mutex::new(CaptureRegion::default()),
            session: Mutex::new(None),
            copy: Mutex::new(None),
            next_reacquire: Mutex::new(Instant::now()),
        }))
    }

    pub fn monitor(gpu: Arc<Gpu>, m: &MonitorTarget, cursor: bool, name: Option<&str>) -> Self {
        let s = Self::make(gpu, &name.map(String::from).unwrap_or(format!("Display {}", m.index + 1)), CaptureMode::Display, 0, m.hmonitor, None, m.index as i32, cursor);
        s.start();
        s
    }

    pub fn window(gpu: Arc<Gpu>, w: &WindowTarget, cursor: bool, name: Option<&str>) -> Self {
        let s = Self::make(gpu, name.unwrap_or(&w.title), CaptureMode::Window, w.hwnd, 0, Some(w.exe_name.clone()), -1, cursor);
        s.start();
        s
    }

    pub fn game(gpu: Arc<Gpu>, exe: &str, cursor: bool, name: Option<&str>) -> Self {
        let s = Self::make(gpu, &name.map(String::from).unwrap_or(format!("Game: {exe}")), CaptureMode::Game, 0, 0, Some(exe.to_string()), -1, cursor);
        s.start();
        s
    }

    pub fn mode(&self) -> CaptureMode {
        self.0.mode
    }
    pub fn exe(&self) -> Option<&str> {
        self.0.exe.as_deref()
    }
    pub fn hwnd(&self) -> isize {
        *self.0.hwnd.lock().unwrap()
    }
    pub fn monitor_index(&self) -> i32 {
        self.0.monitor_index
    }
    pub fn cursor(&self) -> bool {
        self.0.cursor
    }
    pub fn region(&self) -> CaptureRegion {
        *self.0.region.lock().unwrap()
    }
    pub fn set_region(&self, r: CaptureRegion) {
        *self.0.region.lock().unwrap() = r;
    }

    fn start(&self) {
        let i = &self.0;
        let r = match i.mode {
            CaptureMode::Display => attach(&self.0, true, i.hmonitor),
            CaptureMode::Window => {
                let h = *i.hwnd.lock().unwrap();
                if capture::is_window(h) {
                    attach(&self.0, false, h)
                } else {
                    i.base.set_status("Window closed");
                    Ok(())
                }
            }
            CaptureMode::Game => {
                try_acquire_game(&self.0);
                Ok(())
            }
        };
        if let Err(e) = r {
            i.base.set_status(format!("Capture failed: {e}"));
        }
    }
}

fn try_acquire_game(i: &Arc<Inner>) {
    let exe = i.exe.clone().unwrap_or_default();
    match capture::find_window_by_exe(&exe) {
        None => i.base.set_status(format!("Waiting for {exe}")),
        Some(w) if capture::is_minimised(w.hwnd) => i.base.set_status(format!("{exe} is minimised")),
        Some(w) => {
            *i.hwnd.lock().unwrap() = w.hwnd;
            if let Err(e) = attach(i, false, w.hwnd) {
                i.base.set_status(format!("Capture failed: {e}"));
            }
        }
    }
}

fn create_item(handle: isize, monitor: bool) -> Result<GraphicsCaptureItem> {
    let interop = windows::core::factory::<GraphicsCaptureItem, IGraphicsCaptureItemInterop>()?;
    Ok(unsafe {
        if monitor {
            interop.CreateForMonitor(HMONITOR(handle as *mut _))?
        } else {
            interop.CreateForWindow(HWND(handle as *mut _))?
        }
    })
}

fn attach(i: &Arc<Inner>, monitor: bool, handle: isize) -> Result<()> {
    let item = create_item(handle, monitor)?;
    let size = item.Size()?;
    let pool = Direct3D11CaptureFramePool::CreateFreeThreaded(&i.gpu.winrt_device, DirectXPixelFormat::B8G8R8A8UIntNormalized, 3, size)?;
    let weak: Weak<Inner> = Arc::downgrade(i);
    pool.FrameArrived(&TypedEventHandler::new(move |p: &Option<Direct3D11CaptureFramePool>, _| {
        if let (Some(inner), Some(p)) = (weak.upgrade(), p.as_ref()) {
            on_frame(&inner, p);
        }
        Ok(())
    }))?;
    let weak2: Weak<Inner> = Arc::downgrade(i);
    item.Closed(&TypedEventHandler::new(move |_, _| {
        if let Some(inner) = weak2.upgrade() {
            detach(&inner);
            let st = if inner.mode == CaptureMode::Game { format!("Waiting for {}", inner.exe.clone().unwrap_or_default()) } else { "Window closed".into() };
            inner.base.set_status(st);
        }
        Ok(())
    }))?;
    let session = pool.CreateCaptureSession(&item)?;
    session.SetIsCursorCaptureEnabled(i.cursor)?;
    let _ = session.SetIsBorderRequired(false); // older Windows: yellow border stays
    let _ = session.SetMinUpdateInterval(windows::Foundation::TimeSpan::from(Duration::from_millis(1))); // pre-24H2: OS pacing
    session.StartCapture()?;
    let old = i.session.lock().unwrap().replace(Session { item, pool, session, pool_size: size });
    close(old);
    i.base.set_status("Capturing");
    Ok(())
}

fn close(s: Option<Session>) {
    if let Some(s) = s {
        let _ = s.session.Close();
        let _ = s.pool.Close();
        drop(s.item);
    }
}

fn detach(i: &Inner) {
    let s = i.session.lock().unwrap().take();
    close(s);
}

fn on_frame(i: &Inner, pool: &Direct3D11CaptureFramePool) {
    let Ok(frame) = pool.TryGetNextFrame() else { return };
    let Ok(size) = frame.ContentSize() else { return };
    // SystemRelativeTime = QPC time DWM composed this frame, in 100 ns units.
    let qpc = frame.SystemRelativeTime().map(|t| (t.Duration as i128 * crate::util::qpc_freq() as i128 / 10_000_000) as i64).unwrap_or(0);

    // Window resized: rebuild the pool at the new size (the next frame will be right).
    {
        let mut s = i.session.lock().unwrap();
        if let Some(s) = s.as_mut() {
            if size.Width != s.pool_size.Width || size.Height != s.pool_size.Height {
                s.pool_size = size;
                let _ = s.pool.Recreate(&i.gpu.winrt_device, DirectXPixelFormat::B8G8R8A8UIntNormalized, 3, size);
            }
        }
    }
    let tex: Result<ID3D11Texture2D> = (|| {
        let surface = frame.Surface()?;
        let access: IDirect3DDxgiInterfaceAccess = surface.cast()?;
        Ok(unsafe { access.GetInterface::<ID3D11Texture2D>()? })
    })();
    let Ok(src) = tex else { return };
    if copy_frame(i, &src, size.Width.max(0) as u32, size.Height.max(0) as u32).is_ok() {
        i.base.last_frame_qpc.store(qpc, Ordering::Relaxed);
    }
    let _ = frame.Close();
}

fn copy_frame(i: &Inner, src: &ID3D11Texture2D, full_w: u32, full_h: u32) -> Result<()> {
    let (mut x, mut y, mut w, mut h) = (0, 0, full_w, full_h);
    let r = *i.region.lock().unwrap();
    if !r.is_empty() {
        x = r.x.min(full_w.saturating_sub(1));
        y = r.y.min(full_h.saturating_sub(1));
        w = r.width.clamp(1, (full_w - x).max(1));
        h = r.height.clamp(1, (full_h - y).max(1));
    }
    // The source texture can be larger or smaller than the content (pool not yet recreated): never read outside it.
    let mut sd = D3D11_TEXTURE2D_DESC::default();
    unsafe { src.GetDesc(&mut sd) };
    w = w.min(sd.Width.saturating_sub(x));
    h = h.min(sd.Height.saturating_sub(y));
    if w == 0 || h == 0 {
        return Err(anyhow!("empty frame"));
    }
    let ctx = i.gpu.lock();
    let mut c = i.copy.lock().unwrap();
    if c.is_none() || i.base.width.load(Ordering::Relaxed) != w || i.base.height.load(Ordering::Relaxed) != h {
        let tex = i.gpu.texture(w, h, DXGI_FORMAT_B8G8R8A8_UNORM, D3D11_BIND_SHADER_RESOURCE)?;
        let srv = i.gpu.srv(&tex)?;
        *c = Some(Copy { tex, srv });
        i.base.set_size(w, h);
    }
    let b = D3D11_BOX { left: x, top: y, front: 0, right: x + w, bottom: y + h, back: 1 };
    unsafe { ctx.CopySubresourceRegion(&c.as_ref().unwrap().tex, 0, 0, 0, 0, src, 0, Some(&b)) };
    i.base.frames_delivered.fetch_add(1, Ordering::Relaxed);
    Ok(())
}

impl VideoSource for WgcSource {
    fn base(&self) -> &SourceBase {
        &self.0.base
    }
    fn process_id(&self) -> Option<u32> {
        if self.0.mode == CaptureMode::Display {
            return None;
        }
        let h = self.hwnd();
        if h == 0 {
            return None;
        }
        let mut pid = 0u32;
        unsafe { windows::Win32::UI::WindowsAndMessaging::GetWindowThreadProcessId(windows::Win32::Foundation::HWND(h as _), Some(&mut pid)) };
        (pid != 0).then_some(pid)
    }
    fn kind(&self) -> &'static str {
        match self.0.mode {
            CaptureMode::Display => "Display Capture",
            CaptureMode::Window => "Window Capture",
            CaptureMode::Game => "Game Capture",
        }
    }
    fn frame(&self) -> Option<ID3D11ShaderResourceView> {
        self.0.copy.lock().unwrap().as_ref().map(|c| c.srv.clone())
    }
    fn tick(&self) {
        if self.0.mode != CaptureMode::Game || self.0.session.lock().unwrap().is_some() {
            return;
        }
        let mut next = self.0.next_reacquire.lock().unwrap();
        if Instant::now() < *next {
            return;
        }
        *next = Instant::now() + Duration::from_secs(1);
        drop(next);
        try_acquire_game(&self.0);
    }
    fn save(&self) -> serde_json::Value {
        let r = self.region();
        serde_json::json!({
            "kind": "capture",
            "mode": format!("{:?}", self.0.mode),
            "exe": self.0.exe,
            "title": if self.0.mode == CaptureMode::Window { Some(capture::window_title(HWND(self.hwnd() as *mut _))) } else { None },
            "monitor": self.0.monitor_index,
            "cursor": self.0.cursor,
            "region": if r.is_empty() { serde_json::Value::Null } else { serde_json::json!([r.x, r.y, r.width, r.height]) },
        })
    }
    fn shutdown(&self) {
        detach(&self.0);
        let _ctx = self.0.gpu.lock();
        *self.0.copy.lock().unwrap() = None;
    }
}

impl Drop for WgcSource {
    fn drop(&mut self) {
        detach(&self.0);
    }
}
