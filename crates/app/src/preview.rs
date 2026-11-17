//! The preview: a native child window the engine presents the canvas into (DXGI swap chain), placed over
//! the preview area of the egui window. The child is transparent to the mouse (HTTRANSPARENT), so egui
//! receives the input and turns it into scene edits: click selects the top-most item, drag moves it,
//! handles resize it, the top handle rotates it.
use ebs_engine::compositor::{HANDLE_SIZE, ROTATE_HANDLE_OFFSET};
use ebs_engine::scene::{Scene, SceneItem};
use ebs_engine::VideoCanvas;
use eframe::egui::{self, Pos2, Rect, Vec2};
use std::sync::Arc;
use windows::core::w;
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::Graphics::Gdi::HBRUSH;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::WindowsAndMessaging::*;

const HTTRANSPARENT: isize = -1;

unsafe extern "system" fn wndproc(h: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    match msg {
        WM_NCHITTEST => LRESULT(HTTRANSPARENT),
        WM_ERASEBKGND => LRESULT(1),
        _ => DefWindowProcW(h, msg, wp, lp),
    }
}

pub struct PreviewWindow {
    pub hwnd: isize,
    placed: Option<(i32, i32, i32, i32, bool)>,
}

impl PreviewWindow {
    pub fn create(parent: isize) -> anyhow::Result<PreviewWindow> {
        unsafe {
            let inst = GetModuleHandleW(None)?;
            let wc = WNDCLASSW {
                lpfnWndProc: Some(wndproc),
                hInstance: inst.into(),
                lpszClassName: w!("EBSPreview"),
                hCursor: LoadCursorW(None, IDC_ARROW)?,
                hbrBackground: HBRUSH::default(),
                ..Default::default()
            };
            RegisterClassW(&wc);
            // The parent must not paint over the child (egui redraws the whole window every frame).
            let p = HWND(parent as *mut _);
            let style = GetWindowLongPtrW(p, GWL_STYLE);
            SetWindowLongPtrW(p, GWL_STYLE, style | WS_CLIPCHILDREN.0 as isize);
            let h = CreateWindowExW(
                WINDOW_EX_STYLE(0),
                w!("EBSPreview"),
                w!(""),
                WS_CHILD | WS_CLIPSIBLINGS,
                0,
                0,
                16,
                16,
                p,
                None,
                inst,
                None,
            )?;
            Ok(PreviewWindow { hwnd: h.0 as isize, placed: None })
        }
    }

    /// Positions the child in physical pixels; hidden when `visible` is false (e.g. while a popup is open
    /// over the preview area, since a native child always draws above egui).
    pub fn place(&mut self, x: i32, y: i32, w: i32, h: i32, visible: bool) -> bool {
        let want = (x, y, w.max(1), h.max(1), visible);
        if self.placed == Some(want) {
            return false;
        }
        let resized = self.placed.map(|p| (p.2, p.3) != (want.2, want.3)).unwrap_or(true);
        unsafe {
            let hwnd = HWND(self.hwnd as *mut _);
            let _ = SetWindowPos(hwnd, None, x, y, want.2, want.3, SWP_NOZORDER | SWP_NOACTIVATE | if visible { SWP_SHOWWINDOW } else { SWP_HIDEWINDOW });
        }
        self.placed = Some(want);
        resized
    }
}

impl Drop for PreviewWindow {
    fn drop(&mut self) {
        unsafe {
            let _ = DestroyWindow(HWND(self.hwnd as *mut _));
        }
    }
}

#[derive(Clone, Copy, PartialEq)]
enum DragMode {
    Move,
    Resize,
    Rotate,
}

struct Drag {
    item: u64,
    mode: DragMode,
    start_canvas: Pos2,
    item_start: Pos2,
    /// Handle being dragged in item space (0, 0.5 or 1 on each axis).
    hu: f32,
    hv: f32,
    start_w: f32,
    start_h: f32,
    /// The point opposite the handle, which stays fixed on the canvas.
    anchor: Pos2,
}

const HANDLES: [(f32, f32); 8] = [(0.0, 0.0), (0.5, 0.0), (1.0, 0.0), (1.0, 0.5), (1.0, 1.0), (0.5, 1.0), (0.0, 1.0), (0.0, 0.5)];

pub enum PreviewEvent {
    Selected(Option<u64>),
    Changed,
    Context(Option<u64>),
}

#[derive(Default)]
pub struct PreviewEditor {
    drag: Option<Drag>,
}

fn item_to_canvas(it: &SceneItem, u: f32, v: f32) -> Pos2 {
    let (x, y) = it.pixel_transform().apply(u, v);
    Pos2::new(x, y)
}

impl PreviewEditor {
    pub fn dragging(&self) -> bool {
        self.drag.is_some()
    }

    /// Letterbox mapping of the canvas inside `rect` (same as the engine's preview blit).
    fn mapping(rect: Rect, c: VideoCanvas) -> (f32, Vec2) {
        let s = (rect.width() / c.width as f32).min(rect.height() / c.height as f32);
        let off = Vec2::new((rect.width() - c.width as f32 * s) / 2.0, (rect.height() - c.height as f32 * s) / 2.0);
        (s, rect.min.to_vec2() + off)
    }

    fn handle_at(sel: Option<&SceneItem>, p: Pos2, px_scale: f32) -> i32 {
        let Some(it) = sel.filter(|i| i.visible && !i.locked && i.display_width() > 0.0) else { return -1 };
        let tol = (HANDLE_SIZE * 0.9 + 3.0) / px_scale;
        let ry = ROTATE_HANDLE_OFFSET / px_scale / it.display_height();
        if item_to_canvas(it, 0.5, -ry).distance(p) <= tol {
            return 8;
        }
        HANDLES.iter().position(|&(u, v)| item_to_canvas(it, u, v).distance(p) <= tol).map(|i| i as i32).unwrap_or(-1)
    }

    fn cursor_for(handle: i32, it: &SceneItem) -> egui::CursorIcon {
        use egui::CursorIcon::*;
        if handle == 8 {
            return Alias;
        }
        let (u, v) = HANDLES[handle as usize];
        let d = item_to_canvas(it, u, v) - item_to_canvas(it, 0.5, 0.5);
        let ang = (d.y.atan2(d.x).to_degrees() + 360.0 + 22.5) % 180.0;
        match ang {
            a if a < 45.0 => ResizeHorizontal,
            a if a < 90.0 => ResizeNwSe,
            a if a < 135.0 => ResizeVertical,
            _ => ResizeNeSw,
        }
    }

    /// Handles pointer input over the preview area. `selected` is the selected item id.
    pub fn ui(&mut self, ui: &egui::Ui, resp: &egui::Response, rect: Rect, canvas: VideoCanvas, scene: &Arc<Scene>, selected: Option<u64>) -> Vec<PreviewEvent> {
        let mut ev = Vec::new();
        let (s, off) = Self::mapping(rect, canvas);
        if s <= 0.0 {
            return ev;
        }
        let ppp = ui.ctx().pixels_per_point();
        let px_scale = s * ppp;
        let to_canvas = |p: Pos2| Pos2::new((p.x - off.x) / s, (p.y - off.y) / s);
        let sel_item = selected.and_then(|id| scene.item(id));
        let shift = ui.input(|i| i.modifiers.shift);
        let pointer = ui.input(|i| i.pointer.interact_pos().or(i.pointer.hover_pos()));

        if let Some(pp) = pointer.filter(|_| resp.hovered() || self.drag.is_some()) {
            let p = to_canvas(pp);
            if self.drag.is_none() {
                let h = Self::handle_at(sel_item.as_ref(), p, px_scale);
                if h >= 0 {
                    ui.ctx().set_cursor_icon(Self::cursor_for(h, sel_item.as_ref().unwrap()));
                } else if sel_item.as_ref().is_some_and(|i| !i.locked && i.visible && i.hit_test(p.x, p.y)) {
                    ui.ctx().set_cursor_icon(egui::CursorIcon::Move);
                }
            }

            if resp.drag_started_by(egui::PointerButton::Primary) || (resp.clicked() && self.drag.is_none()) {
                let press = ui.input(|i| i.pointer.press_origin()).unwrap_or(pp);
                let pc = to_canvas(press);
                let h = Self::handle_at(sel_item.as_ref(), pc, px_scale);
                if h >= 0 {
                    let it = sel_item.clone().unwrap();
                    let (mode, hu, hv, anchor) = if h == 8 {
                        (DragMode::Rotate, 0.5, 0.5, Pos2::ZERO)
                    } else {
                        let (hu, hv) = HANDLES[h as usize];
                        (DragMode::Resize, hu, hv, item_to_canvas(&it, 1.0 - hu, 1.0 - hv))
                    };
                    self.drag = Some(Drag { item: it.id, mode, start_canvas: pc, item_start: Pos2::new(it.x, it.y), hu, hv, start_w: it.display_width(), start_h: it.display_height(), anchor });
                } else {
                    // Keep dragging the selected item if the click is on it, even when something is stacked above.
                    let hit = match &sel_item {
                        Some(i) if i.visible && i.hit_test(pc.x, pc.y) => Some(i.clone()),
                        _ => hit_test(scene, pc),
                    };
                    ev.push(PreviewEvent::Selected(hit.as_ref().map(|i| i.id)));
                    if let Some(it) = hit.filter(|i| !i.locked) {
                        if resp.drag_started_by(egui::PointerButton::Primary) {
                            self.drag = Some(Drag { item: it.id, mode: DragMode::Move, start_canvas: pc, item_start: Pos2::new(it.x, it.y), hu: 0.0, hv: 0.0, start_w: 0.0, start_h: 0.0, anchor: Pos2::ZERO });
                        }
                    }
                }
            }

            if let Some(d) = &self.drag {
                if resp.dragged_by(egui::PointerButton::Primary) {
                    let d_item = d.item;
                    let mode = d.mode;
                    scene.update(d_item, |it| match mode {
                        DragMode::Move => {
                            let mut nx = d.item_start.x + (p.x - d.start_canvas.x);
                            let mut ny = d.item_start.y + (p.y - d.start_canvas.y);
                            // Snap to canvas edges within 12 px.
                            let (cw, ch) = (canvas.width as f32, canvas.height as f32);
                            if nx.abs() < 12.0 {
                                nx = 0.0;
                            }
                            if ny.abs() < 12.0 {
                                ny = 0.0;
                            }
                            if (nx + it.display_width() - cw).abs() < 12.0 {
                                nx = cw - it.display_width();
                            }
                            if (ny + it.display_height() - ch).abs() < 12.0 {
                                ny = ch - it.display_height();
                            }
                            it.x = nx.round();
                            it.y = ny.round();
                            it.auto_fit = false;
                        }
                        DragMode::Resize => resize(it, d, p, shift),
                        DragMode::Rotate => {
                            let c = item_to_canvas(it, 0.5, 0.5);
                            let mut deg = (p.y - c.y).atan2(p.x - c.x).to_degrees() + 90.0;
                            deg = ((deg % 360.0) + 360.0) % 360.0;
                            // Shift snaps to 15 degree steps; without it, straight angles still snap within 3 degrees.
                            let step = if shift { 15.0 } else { 90.0 };
                            let snapped = (deg / step).round() * step;
                            if shift || (snapped - deg).abs() < 3.0 {
                                deg = snapped % 360.0;
                            }
                            it.rotation = (deg * 10.0).round() / 10.0;
                            it.auto_fit = false;
                        }
                    });
                }
            }
        }
        if self.drag.is_some() && resp.drag_stopped() {
            self.drag = None;
            ev.push(PreviewEvent::Changed);
        }
        if resp.secondary_clicked() {
            let hit = pointer.and_then(|pp| hit_test(scene, to_canvas(pp)));
            ev.push(PreviewEvent::Selected(hit.as_ref().map(|i| i.id)));
            ev.push(PreviewEvent::Context(hit.map(|i| i.id)));
        }
        ev
    }
}

fn hit_test(scene: &Scene, p: Pos2) -> Option<SceneItem> {
    scene.snapshot().into_iter().rev().find(|i| i.visible && i.source.size().0 > 0 && i.hit_test(p.x, p.y))
}

/// Resizes from a handle while the opposite point stays fixed on the canvas (works for rotated items).
/// Corner handles keep the aspect ratio unless Shift is held; edge handles stretch one side.
fn resize(it: &mut SceneItem, d: &Drag, mouse: Pos2, free: bool) {
    let (cw, ch) = (it.cropped_width() as f32, it.cropped_height() as f32);
    if cw <= 0.0 || ch <= 0.0 {
        return;
    }
    let rad = it.rotation.to_radians();
    let ux = Vec2::new(rad.cos(), rad.sin());
    let uy = Vec2::new(-rad.sin(), rad.cos());
    let v = mouse - d.anchor;
    let (mut w, mut h) = (d.start_w, d.start_h);
    let corner = d.hu != 0.5 && d.hv != 0.5;
    if d.hu != 0.5 {
        w = (v.dot(ux) * if d.hu > 0.5 { 1.0 } else { -1.0 }).max(8.0);
    }
    if d.hv != 0.5 {
        h = (v.dot(uy) * if d.hv > 0.5 { 1.0 } else { -1.0 }).max(8.0);
    }
    if corner && !free {
        let k = (w / d.start_w).max(h / d.start_h);
        w = d.start_w * k;
        h = d.start_h * k;
    }
    it.scale_x = w / cw;
    it.scale_y = h / ch;
    // Put the centre where it must be for the anchor (opposite point) to stay fixed.
    let (au, av) = (1.0 - d.hu, 1.0 - d.hv);
    let (ox, oy) = ((0.5 - au) * w, (0.5 - av) * h);
    let c = d.anchor + ux * ox + uy * oy;
    it.x = (c.x - w / 2.0).round();
    it.y = (c.y - h / 2.0).round();
    it.auto_fit = false;
}
