use eframe::egui::{self, Color32, ColorImage, TextureHandle, TextureOptions};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
    mpsc,
};

pub struct CursorTracker {
    stop: Arc<AtomicBool>,
    pub active: Arc<AtomicBool>,
    rx: mpsc::Receiver<([f64; 2], [f64; 2], bool)>,
}
impl CursorTracker {
    pub fn start(host: String, port: u16, token: String) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let active = Arc::new(AtomicBool::new(false));
        let (tx, rx) = mpsc::sync_channel(1);
        let worker_stop = stop.clone();
        let worker_active = active.clone();
        std::thread::spawn(move || {
            // A tracker belongs to one capture session. Never apply coordinates
            // from a later host target to the current framebuffer.
            let mut generation = None;
            while !worker_stop.load(Ordering::Relaxed) {
                if worker_active.load(Ordering::Relaxed) {
                    if let Ok((_, bytes)) = crate::window_picker::request(
                        &host,
                        port,
                        &token,
                        "GET",
                        "/cursor",
                        "",
                        &[],
                    ) {
                        if let Ok(v) = serde_json::from_slice::<serde_json::Value>(&bytes) {
                            if let (Some(x), Some(y), Some(w), Some(h), Some(current)) = (
                                v["x"].as_f64(),
                                v["y"].as_f64(),
                                v["logical_size"][0].as_f64(),
                                v["logical_size"][1].as_f64(),
                                v["generation"].as_u64(),
                            ) {
                                let expected = *generation.get_or_insert(current);
                                if current == expected
                                    && w > 0.0
                                    && h > 0.0
                                    && worker_active.load(Ordering::Relaxed)
                                    && !worker_stop.load(Ordering::Relaxed)
                                {
                                    let _ = tx.try_send((
                                        [x, y],
                                        [w, h],
                                        v["visible"].as_bool().unwrap_or(false),
                                    ));
                                }
                            }
                        }
                    }
                }
                std::thread::sleep(std::time::Duration::from_millis(150));
            }
        });
        Self { stop, active, rx }
    }
    pub fn set_active(&self, active: bool) {
        self.active.store(active, Ordering::Relaxed);
        if !active {
            while self.rx.try_recv().is_ok() {}
        }
    }
    pub fn poll(&self, size: [u32; 2]) -> Option<([u16; 2], bool)> {
        let mut latest = None;
        while let Ok((point, logical, visible)) = self.rx.try_recv() {
            let scale =
                (size[0] as f64 / logical[0].max(1.0)).min(size[1] as f64 / logical[1].max(1.0));
            let content = [logical[0] * scale, logical[1] * scale];
            let px = (size[0] as f64 - content[0]) / 2.0 + point[0] * content[0];
            let py = (size[1] as f64 - content[1]) / 2.0 + point[1] * content[1];
            latest = Some((
                [
                    px.clamp(0.0, size[0].saturating_sub(1) as f64) as u16,
                    py.clamp(0.0, size[1].saturating_sub(1) as f64) as u16,
                ],
                visible,
            ));
        }
        latest
    }
}
impl Drop for CursorTracker {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

pub enum CursorUpdate {
    Default,
    Hidden,
    Position([u16; 2]),
    Bitmap {
        size: [usize; 2],
        hotspot: [u16; 2],
        rgba: Vec<u8>,
    },
}

#[derive(Default)]
pub struct RemoteCursor {
    position: Option<[u16; 2]>,
    hidden: bool,
    texture: Option<TextureHandle>,
    hotspot: [u16; 2],
    pub outside_target: bool,
}

impl RemoteCursor {
    pub fn update(&mut self, ctx: &egui::Context, update: CursorUpdate) -> Option<[u16; 2]> {
        match update {
            CursorUpdate::Default => {
                self.hidden = false;
                self.texture = None;
            }
            CursorUpdate::Hidden => self.hidden = true,
            CursorUpdate::Position(point) => {
                self.position = Some(point);
                return Some(point);
            }
            CursorUpdate::Bitmap {
                size,
                hotspot,
                rgba,
            } => {
                if size.contains(&0) {
                    self.hidden = true;
                } else if size[0] <= 384 && size[1] <= 384 && size[0] * size[1] * 4 == rgba.len() {
                    self.texture = Some(ctx.load_texture(
                        "remote-cursor",
                        ColorImage::from_rgba_unmultiplied(size, &rgba),
                        TextureOptions::LINEAR,
                    ));
                    self.hotspot = hotspot;
                    self.hidden = false;
                }
            }
        }
        None
    }

    pub fn paint(
        &self,
        ui: &egui::Ui,
        rect: egui::Rect,
        uv: egui::Rect,
        size: [u32; 2],
        predicted: Option<[u16; 2]>,
    ) {
        if self.hidden || self.outside_target || size.contains(&0) {
            return;
        }
        let Some(point) = predicted.or(self.position) else {
            return;
        };
        let normalized = egui::pos2(
            point[0] as f32 / size[0].saturating_sub(1).max(1) as f32,
            point[1] as f32 / size[1].saturating_sub(1).max(1) as f32,
        );
        if !uv.contains(normalized) {
            return;
        }
        let pos = rect.min + (normalized - uv.min) / uv.size() * rect.size();
        let painter = ui.painter().with_clip_rect(rect);
        if let Some(texture) = &self.texture {
            let scale = egui::vec2(
                rect.width() / (uv.width() * size[0] as f32),
                rect.height() / (uv.height() * size[1] as f32),
            );
            let origin = pos - egui::vec2(self.hotspot[0] as f32, self.hotspot[1] as f32) * scale;
            painter.image(
                texture.id(),
                egui::Rect::from_min_size(origin, texture.size_vec2() * scale),
                egui::Rect::from_min_max(egui::Pos2::ZERO, egui::pos2(1.0, 1.0)),
                Color32::WHITE,
            );
        } else {
            let points = vec![
                pos,
                pos + egui::vec2(0.0, 21.0),
                pos + egui::vec2(5.0, 16.0),
                pos + egui::vec2(9.0, 25.0),
                pos + egui::vec2(13.0, 23.0),
                pos + egui::vec2(9.0, 14.0),
                pos + egui::vec2(17.0, 14.0),
            ];
            // Concave arrow uses an outline path plus a triangular pointer.
            painter.add(egui::Shape::convex_polygon(
                vec![points[0], points[1], points[6]],
                Color32::WHITE,
                egui::Stroke::new(1.3, Color32::BLACK),
            ));
        }
    }
}
