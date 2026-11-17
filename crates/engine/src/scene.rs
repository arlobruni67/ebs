//! Scenes and scene items (position, scale, rotation, crop, opacity, visibility, lock, fit).
use crate::math::Mat4;
use crate::sources::VideoSource;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

static NEXT_ID: AtomicU64 = AtomicU64::new(1);
pub fn next_id() -> u64 {
    NEXT_ID.fetch_add(1, Ordering::Relaxed)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum FitMode {
    #[default]
    Contain,
    Fill,
}

#[derive(Clone)]
pub struct SceneItem {
    pub id: u64,
    pub source: Arc<dyn VideoSource>,
    pub visible: bool,
    pub locked: bool,
    pub x: f32,
    pub y: f32,
    pub scale_x: f32,
    pub scale_y: f32,
    /// Degrees, clockwise on screen.
    pub rotation: f32,
    pub crop_left: u32,
    pub crop_top: u32,
    pub crop_right: u32,
    pub crop_bottom: u32,
    pub opacity: f32,
    /// Re-fit to the canvas whenever the source or canvas size changes (until the user moves it).
    pub auto_fit: bool,
    pub fit: FitMode,
    pub(crate) fitted: Option<(u32, u32, u32, u32, FitMode)>,
}

impl SceneItem {
    pub fn new(source: Arc<dyn VideoSource>) -> Self {
        Self {
            id: next_id(),
            source,
            visible: true,
            locked: false,
            x: 0.0,
            y: 0.0,
            scale_x: 1.0,
            scale_y: 1.0,
            rotation: 0.0,
            crop_left: 0,
            crop_top: 0,
            crop_right: 0,
            crop_bottom: 0,
            opacity: 1.0,
            auto_fit: true,
            fit: FitMode::Contain,
            fitted: None,
        }
    }

    pub fn cropped_width(&self) -> u32 {
        self.source.size().0.saturating_sub(self.crop_left + self.crop_right)
    }
    pub fn cropped_height(&self) -> u32 {
        self.source.size().1.saturating_sub(self.crop_top + self.crop_bottom)
    }
    pub fn display_width(&self) -> f32 {
        self.cropped_width() as f32 * self.scale_x
    }
    pub fn display_height(&self) -> f32 {
        self.cropped_height() as f32 * self.scale_y
    }

    pub fn fit_to_canvas(&mut self, cw: u32, ch: u32) {
        let (w, h) = (self.cropped_width(), self.cropped_height());
        if w == 0 || h == 0 {
            return;
        }
        let (sx, sy) = (cw as f32 / w as f32, ch as f32 / h as f32);
        let s = if self.fit == FitMode::Fill { sx.max(sy) } else { sx.min(sy) };
        self.scale_x = s;
        self.scale_y = s;
        self.x = (cw as f32 - w as f32 * s) / 2.0;
        self.y = (ch as f32 - h as f32 * s) / 2.0;
        self.rotation = 0.0;
    }

    /// Unit quad (0..1) to canvas pixels: scale, rotate about the centre, place.
    pub fn pixel_transform(&self) -> Mat4 {
        let (w, h) = (self.display_width(), self.display_height());
        Mat4::scale(w, h) * Mat4::translate(-w / 2.0, -h / 2.0) * Mat4::rotate_z(self.rotation.to_radians()) * Mat4::translate(self.x + w / 2.0, self.y + h / 2.0)
    }

    /// Point in canvas pixels to the item's unit space (None if degenerate).
    pub fn to_local(&self, px: f32, py: f32) -> Option<(f32, f32)> {
        self.pixel_transform().invert_2d().map(|m| m.apply(px, py))
    }

    pub fn hit_test(&self, px: f32, py: f32) -> bool {
        matches!(self.to_local(px, py), Some((u, v)) if (0.0..=1.0).contains(&u) && (0.0..=1.0).contains(&v))
    }
}

pub struct Scene {
    pub id: u64,
    name: Mutex<String>,
    items: Mutex<Vec<SceneItem>>,
}

impl Scene {
    pub fn new(name: &str) -> Arc<Scene> {
        Arc::new(Scene { id: next_id(), name: Mutex::new(name.to_string()), items: Mutex::new(Vec::new()) })
    }
    pub fn name(&self) -> String {
        self.name.lock().unwrap().clone()
    }
    pub fn set_name(&self, n: &str) {
        *self.name.lock().unwrap() = n.to_string();
    }

    /// Adds a source on top; returns the item id.
    pub fn add(&self, source: Arc<dyn VideoSource>) -> u64 {
        let item = SceneItem::new(source);
        let id = item.id;
        self.items.lock().unwrap().push(item);
        id
    }
    pub fn add_item(&self, item: SceneItem) {
        self.items.lock().unwrap().push(item);
    }
    pub fn remove(&self, id: u64) {
        self.items.lock().unwrap().retain(|i| i.id != id);
    }
    pub fn remove_source(&self, source_id: u64) {
        self.items.lock().unwrap().retain(|i| i.source.id() != source_id);
    }
    /// Moves an item up (+) or down (-) the draw order (index 0 is drawn first, at the back).
    pub fn move_by(&self, id: u64, delta: i32) {
        let mut v = self.items.lock().unwrap();
        if let Some(i) = v.iter().position(|x| x.id == id) {
            let j = (i as i32 + delta).clamp(0, v.len() as i32 - 1) as usize;
            if i != j {
                let it = v.remove(i);
                v.insert(j, it);
            }
        }
    }
    pub fn update<R>(&self, id: u64, f: impl FnOnce(&mut SceneItem) -> R) -> Option<R> {
        self.items.lock().unwrap().iter_mut().find(|i| i.id == id).map(f)
    }
    pub fn item(&self, id: u64) -> Option<SceneItem> {
        self.items.lock().unwrap().iter().find(|i| i.id == id).cloned()
    }
    pub fn snapshot(&self) -> Vec<SceneItem> {
        self.items.lock().unwrap().clone()
    }

    /// Re-fits auto-fit items whose source or canvas size changed. Called by the render loop.
    pub fn apply_auto_fit(&self, cw: u32, ch: u32) {
        for it in self.items.lock().unwrap().iter_mut() {
            if !it.auto_fit {
                continue;
            }
            let (sw, sh) = it.source.size();
            if sw == 0 {
                continue;
            }
            let key = (sw, sh, cw, ch, it.fit);
            if it.fitted == Some(key) {
                continue;
            }
            it.fit_to_canvas(cw, ch);
            it.fitted = Some(key);
        }
    }
}
