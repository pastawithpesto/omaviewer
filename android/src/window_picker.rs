//! Async egui window picker backed by the authenticated WindowLink host control API.

use crate::theme::Palette;
use eframe::egui::{self, RichText};
use serde_json::Value;

use std::{
    collections::{HashMap, HashSet},
    io::{Read, Write},
    net::TcpStream,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
        mpsc::{self, Receiver, SyncSender},
    },
    thread,
    time::{Duration, Instant},
};

#[derive(Clone, Debug)]
pub struct WindowInfo {
    pub address: String,
    pub stable_id: String,
    pub title: String,
    pub class: String,
    pub monitor: String,
    pub workspace: String,
    /// `[x, y]` position on the monitor, in desktop pixels.
    pub at: Option<[f64; 2]>,
    /// `[width, height]` in desktop pixels.
    pub size: Option<[f64; 2]>,
}

impl WindowInfo {
    fn from_json(value: &serde_json::Value) -> Result<Self, String> {
        let address = value
            .get("address")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| "window entry has no address".to_owned())?;
        let text = |key: &str| -> String { value.get(key).map(display_value).unwrap_or_default() };
        let pair = |key: &str| -> Option<[f64; 2]> {
            let values = value.get(key)?.as_array()?;
            let numbers: Vec<f64> = values.iter().filter_map(Value::as_f64).collect();
            if numbers.len() == 2 {
                Some([numbers[0], numbers[1]])
            } else {
                None
            }
        };
        Ok(Self {
            address: address.to_owned(),
            stable_id: text("stable_id"),
            title: text("title"),
            class: text("class"),
            monitor: text("monitor"),
            workspace: value
                .get("workspace")
                .and_then(|workspace| workspace.get("name"))
                .and_then(serde_json::Value::as_str)
                .unwrap_or("?")
                .to_owned(),
            at: pair("at"),
            size: pair("size"),
        })
    }
}

#[derive(Debug)]
enum PickerEvent {
    Outputs(Vec<(String, String)>),
    HostGeneration(u64),
    MonitorSelected(Result<String, String>),
    Listed {
        generation: u64,
        result: Result<Vec<WindowInfo>, String>,
    },
    Selected {
        result: Result<Option<WindowInfo>, String>,
    },
    Preview {
        generation: u64,
        address: String,
        result: Result<DecodedPreview, String>,
    },
    CycleFinished {
        generation: u64,
    },
}

#[derive(Debug)]
struct DecodedPreview {
    size: [usize; 2],
    rgba: Vec<u8>,
}

/// State and UI for selecting a window on the host's currently streamed monitor.
/// Call `open_and_refresh` from a button, then `ui` once per frame.
pub struct WindowPicker {
    loading: bool,
    outputs: Vec<(String, String)>,
    monitor: String,
    windows: Vec<WindowInfo>,
    message: String,
    selected: Option<String>,
    selection_known: bool,
    display_changed: bool,
    pub host_generation: Option<u64>,
    tx: SyncSender<PickerEvent>,
    rx: Receiver<PickerEvent>,
    previews: HashMap<String, egui::TextureHandle>,
    last_refresh: Instant,
    preview_interval_secs: u64,
    generation: u64,
    generation_signal: Arc<AtomicU64>,
    worker_generation: Option<u64>,
    /// `x, y, w, h` of the selected window in desktop pixels, when isolated.
    selected_geometry: Option<[f64; 4]>,
}

impl Default for WindowPicker {
    fn default() -> Self {
        let (tx, rx) = mpsc::sync_channel(1);
        Self {
            loading: false,
            outputs: Vec::new(),
            monitor: String::new(),
            windows: Vec::new(),
            message: String::new(),
            selected: None,
            selection_known: false,
            display_changed: false,
            host_generation: None,
            tx,
            rx,
            previews: HashMap::new(),
            last_refresh: Instant::now(),
            preview_interval_secs: 30,
            generation: 0,
            generation_signal: Arc::new(AtomicU64::new(0)),
            worker_generation: None,
            selected_geometry: None,
        }
    }
}

impl WindowPicker {
    pub fn target_identity(&self) -> Value {
        if let Some(address) = &self.selected {
            if let Some(window) = self
                .windows
                .iter()
                .find(|window| &window.address == address)
            {
                return serde_json::json!({"kind":"window", "address":window.address, "stable_id":window.stable_id, "output":window.monitor});
            }
        }
        serde_json::json!({"kind":"monitor", "output":self.monitor})
    }

    pub fn take_display_changed(&mut self) -> bool {
        std::mem::take(&mut self.display_changed)
    }

    /// Snapshot cadence for unopened window thumbnails. Zero disables automatic
    /// refresh; explicit Refresh actions remain available.
    pub fn preview_interval_secs(&self) -> u64 {
        self.preview_interval_secs
    }

    /// Configure automatic thumbnail snapshots. Nonzero values are bounded to
    /// 5–300 seconds. Changing the setting invalidates an in-flight generation.
    pub fn set_preview_interval_secs(&mut self, seconds: u64) {
        let seconds = if seconds == 0 {
            0
        } else {
            seconds.clamp(5, 300)
        };
        if seconds == self.preview_interval_secs {
            return;
        }
        self.preview_interval_secs = seconds;
        self.loading = false;
        self.generation = self.generation.wrapping_add(1);
        self.generation_signal
            .store(self.generation, Ordering::Relaxed);
        // Make the next automatic pass eligible as soon as an old worker exits.
        self.last_refresh = Instant::now() - Duration::from_secs(seconds);
    }

    /// Clear picker state when the connection changes. Replacing the receiver
    /// makes results from old workers unreachable; preserve configured cadence.
    pub fn reset(&mut self) {
        let interval = self.preview_interval_secs;
        self.generation_signal
            .store(self.generation.wrapping_add(1), Ordering::Relaxed);
        *self = Self {
            preview_interval_secs: interval,
            ..Self::default()
        };
    }

    pub fn refresh(&mut self, control_host: &str, control_port: u16, token: &str) {
        if self.worker_generation.is_some() {
            return;
        }
        self.generation = self.generation.wrapping_add(1);
        let generation = self.generation;
        self.generation_signal.store(generation, Ordering::Relaxed);
        self.worker_generation = Some(generation);
        self.last_refresh = Instant::now();
        self.loading = true;
        self.message = "Loading windows…".into();
        let host = control_host.to_owned();
        let token = token.to_owned();
        let tx = self.tx.clone();
        let generation_signal = Arc::clone(&self.generation_signal);
        thread::spawn(move || {
            if let Ok((_, bytes)) = request(&host, control_port, &token, "GET", "/outputs", "", &[])
            {
                if let Ok(Value::Array(items)) = serde_json::from_slice::<Value>(&bytes) {
                    let outputs = items
                        .iter()
                        .filter_map(|m| {
                            Some((
                                m["name"].as_str()?.to_owned(),
                                m["description"].as_str().unwrap_or("").to_owned(),
                            ))
                        })
                        .collect();
                    if generation_signal.load(Ordering::Relaxed) == generation {
                        let _ = tx.send(PickerEvent::Outputs(outputs));
                    }
                }
            }
            let result = list_windows(&host, control_port, &token);
            if generation_signal.load(Ordering::Relaxed) == generation {
                let windows = result.as_ref().ok().cloned().unwrap_or_default();
                let _ = tx.send(PickerEvent::Listed { generation, result });
                for window in windows.iter().take(24) {
                    if generation_signal.load(Ordering::Relaxed) != generation {
                        break;
                    }
                    let address = window.address.clone();
                    let result = load_preview(&host, control_port, &token, &address)
                        .and_then(decode_preview);
                    if generation_signal.load(Ordering::Relaxed) != generation {
                        break;
                    }
                    if tx
                        .send(PickerEvent::Preview {
                            generation,
                            address,
                            result,
                        })
                        .is_err()
                    {
                        break;
                    }
                }
            }
            let _ = tx.send(PickerEvent::CycleFinished { generation });
        });
    }

    fn select(
        &mut self,
        control_host: &str,
        control_port: u16,
        token: &str,
        address: Option<String>,
    ) {
        self.loading = true;
        self.message = "Changing shared window…".into();
        let host = control_host.to_owned();
        let token = token.to_owned();
        let tx = self.tx.clone();
        let stable_id = address
            .as_ref()
            .and_then(|address| {
                self.windows
                    .iter()
                    .find(|window| &window.address == address)
            })
            .map(|window| window.stable_id.clone())
            .filter(|id| !id.is_empty());
        thread::spawn(move || {
            let result = select_window(
                &host,
                control_port,
                &token,
                address.as_deref(),
                stable_id.as_deref(),
            );
            let result = result.map(|(window, generation)| {
                let _ = tx.send(PickerEvent::HostGeneration(generation));
                window
            });
            let _ = tx.send(PickerEvent::Selected { result });
        });
    }

    pub fn poll(&mut self, ctx: &egui::Context) {
        while let Ok(event) = self.rx.try_recv() {
            match event {
                PickerEvent::Outputs(outputs) => self.outputs = outputs,
                PickerEvent::HostGeneration(generation) => self.host_generation = Some(generation),
                PickerEvent::MonitorSelected(result) => {
                    self.loading = false;
                    match result {
                        Ok(monitor) => {
                            self.monitor = monitor;
                            self.selected = None;
                            self.selection_known = true;
                            self.selected_geometry = None;
                            self.display_changed = true;
                        }
                        Err(error) => self.message = error,
                    }
                }
                PickerEvent::Listed {
                    generation,
                    result: Ok(windows),
                } if generation == self.generation => {
                    self.loading = false;
                    let addresses: HashSet<_> = windows
                        .iter()
                        .map(|window| window.address.as_str())
                        .collect();
                    self.previews
                        .retain(|address, _| addresses.contains(address.as_str()));
                    self.windows = windows;
                    self.message = format!("{} windows", self.windows.len());
                }
                PickerEvent::Listed {
                    generation,
                    result: Err(error),
                } if generation == self.generation => {
                    self.loading = false;
                    self.message = error;
                }
                PickerEvent::Selected { result: Err(error) } => {
                    self.loading = false;
                    self.message = error;
                }
                PickerEvent::Selected {
                    result: Ok(Some(window)),
                } => {
                    self.loading = false;
                    self.message = format!("Sharing: {}", window.title);
                    self.selected = Some(window.address);
                    self.selection_known = true;
                    self.display_changed = true;
                    self.selected_geometry = match (window.at, window.size) {
                        (Some([x, y]), Some([w, h])) if w > 1.0 && h > 1.0 => Some([x, y, w, h]),
                        _ => None,
                    };
                }
                PickerEvent::Selected { result: Ok(None) } => {
                    self.loading = false;
                    self.message = "Sharing the desktop".into();
                    self.selected = None;
                    self.selection_known = true;
                    self.display_changed = true;
                    self.selected_geometry = None;
                }
                PickerEvent::Preview {
                    generation,
                    address,
                    result: Ok(decoded),
                } if generation == self.generation
                    && self.windows.iter().any(|window| window.address == address) =>
                {
                    let color =
                        egui::ColorImage::from_rgba_unmultiplied(decoded.size, &decoded.rgba);
                    let texture = ctx.load_texture(
                        format!("window-preview-{address}"),
                        color,
                        egui::TextureOptions::LINEAR,
                    );
                    self.previews.insert(address, texture);
                }
                PickerEvent::Preview { .. } => {}
                PickerEvent::CycleFinished { generation }
                    if self.worker_generation == Some(generation) =>
                {
                    self.worker_generation = None;
                    if generation == self.generation {
                        self.last_refresh = Instant::now();
                    }
                }
                PickerEvent::CycleFinished { .. } => {}
                PickerEvent::Listed { .. } => {}
            }
        }
    }

    pub fn count(&self) -> usize {
        self.windows.len()
    }

    /// Viewport settings that isolate the selected window on the streamed monitor:
    /// center on it and zoom so it fills the view. `None` when no window with known
    /// geometry is selected, meaning the whole monitor should be shown.
    pub fn viewport_focus(
        &self,
        desktop: &windowlink_shared::DesktopSize,
    ) -> Option<(windowlink_shared::NormalizedPoint, f64)> {
        let [x, y, width, height] = self.selected_geometry?;
        if x.is_finite() && y.is_finite() && width.is_finite() && height.is_finite() {
            let desktop_width = desktop.width as f64;
            let desktop_height = desktop.height as f64;
            if desktop_width < 1.0 || desktop_height < 1.0 {
                return None;
            }
            let zoom = (desktop_width / width).min(desktop_height / height);
            if !zoom.is_finite() || zoom < 1.0 {
                return None;
            }
            let center = windowlink_shared::NormalizedPoint::new(
                unit((x + width / 2.0) / desktop_width),
                unit((y + height / 2.0) / desktop_height),
            );
            return Some((center, zoom));
        }
        None
    }

    pub fn ui(
        &mut self,
        ui: &mut egui::Ui,
        palette: Palette,
        control_host: &str,
        control_port: u16,
        token: &str,
    ) {
        if self.preview_interval_secs != 0
            && self.worker_generation.is_none()
            && self.last_refresh.elapsed() >= Duration::from_secs(self.preview_interval_secs)
        {
            self.refresh(control_host, control_port, token);
        }
        let mut selected_output = None;
        if !self.outputs.is_empty() {
            ui.horizontal(|ui| {
                ui.label("Monitor");
                egui::ComboBox::from_id_salt("monitor-selector")
                    .selected_text(if self.monitor.is_empty() {
                        "Choose monitor"
                    } else {
                        &self.monitor
                    })
                    .show_ui(ui, |ui| {
                        for (name, description) in &self.outputs {
                            if ui
                                .selectable_label(name == &self.monitor, name)
                                .on_hover_text(description)
                                .clicked()
                            {
                                selected_output = Some(name.clone());
                            }
                        }
                    });
            });
        }
        if let Some(name) = selected_output.filter(|_| !self.loading) {
            self.loading = true;
            let host = control_host.to_owned();
            let token = token.to_owned();
            let tx = self.tx.clone();
            thread::spawn(move || {
                let bytes = serde_json::json!({"name":name}).to_string().into_bytes();
                let result = request(
                    &host,
                    control_port,
                    &token,
                    "POST",
                    "/monitor",
                    "application/json",
                    &bytes,
                )
                .map(|(_, body)| {
                    if let Ok(value) = serde_json::from_slice::<Value>(&body) {
                        if let Some(generation) = value["generation"].as_u64() {
                            let _ = tx.send(PickerEvent::HostGeneration(generation));
                        }
                    }
                    name
                });
                let _ = tx.send(PickerEvent::MonitorSelected(result));
            });
        }
        let mut action: Option<Option<String>> = None;
        ui.horizontal(|ui| {
            ui.label(
                RichText::new("WINDOWS")
                    .size(16.0)
                    .strong()
                    .color(palette.accent),
            );
            ui.label(
                RichText::new(format!("{:02}", self.windows.len()))
                    .size(15.0)
                    .color(palette.muted),
            );
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.button("Refresh").clicked() && self.worker_generation.is_none() {
                    self.refresh(control_host, control_port, token);
                }
            });
        });
        if self.message.starts_with("Could not") || self.message.starts_with("Failed") {
            ui.label(RichText::new(&self.message).small().color(palette.muted));
        }
        if self.loading {
            ui.spinner();
        }
        ui.add_space(8.0);
        if window_row(
            ui,
            palette,
            "whole-monitor",
            "Whole monitor",
            "Show every window",
            self.selection_known && self.selected.is_none(),
            None,
        )
        .clicked()
            && !self.loading
        {
            action = Some(None);
        }
        ui.add_space(8.0);
        for window in &self.windows {
            let name = if window.title.is_empty() {
                "Untitled window"
            } else {
                &window.title
            };
            let subtitle = format!("{}  ·  Workspace {}", window.class, window.workspace);
            if window_row(
                ui,
                palette,
                &window.address,
                name,
                &subtitle,
                self.selection_known && self.selected.as_deref() == Some(window.address.as_str()),
                self.previews.get(&window.address),
            )
            .clicked()
                && !self.loading
            {
                action = Some(Some(window.address.clone()));
            }
            ui.add_space(8.0);
        }
        if let Some(address) = action {
            self.select(control_host, control_port, token, address);
        }
    }
}

fn window_row(
    ui: &mut egui::Ui,
    palette: Palette,
    id: &str,
    title: &str,
    detail: &str,
    active: bool,
    preview: Option<&egui::TextureHandle>,
) -> egui::Response {
    let width = ui.available_width();
    let title = shorten(title, 42);
    let detail = shorten(detail, 50);
    let row = egui::Frame::new()
        .fill(if active {
            palette.raised
        } else {
            palette.surface
        })
        .stroke(egui::Stroke::new(
            1.0,
            if active {
                palette.accent
            } else {
                palette.border
            },
        ))
        .corner_radius(12.0)
        .inner_margin(egui::Margin::symmetric(14, 12))
        .show(ui, |ui| {
            ui.set_min_width((width - 28.0).max(0.0));
            ui.set_min_height(70.0);
            ui.horizontal(|ui| {
                ui.vertical(|ui| {
                    ui.set_max_width((width - 138.0).max(80.0));
                    ui.label(RichText::new(title).size(18.0).strong().color(palette.text));
                    ui.label(RichText::new(detail).size(14.0).color(palette.muted));
                });
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    let (rect, _) =
                        ui.allocate_exact_size(egui::vec2(96.0, 58.0), egui::Sense::hover());
                    ui.painter().rect_filled(rect, 6.0, palette.background);
                    if let Some(texture) = preview {
                        let size = texture.size_vec2();
                        let scale = (rect.width() / size.x).min(rect.height() / size.y);
                        let fitted = egui::Rect::from_center_size(rect.center(), size * scale);
                        ui.painter().image(
                            texture.id(),
                            fitted,
                            egui::Rect::from_min_max(egui::Pos2::ZERO, egui::pos2(1.0, 1.0)),
                            egui::Color32::WHITE,
                        );
                    } else {
                        ui.painter().text(
                            rect.center(),
                            egui::Align2::CENTER_CENTER,
                            "PREVIEW",
                            egui::FontId::proportional(10.0),
                            palette.muted,
                        );
                    }
                });
            });
        });
    ui.interact(row.response.rect, ui.id().with(id), egui::Sense::click())
        .on_hover_cursor(egui::CursorIcon::PointingHand)
}

fn load_preview(host: &str, port: u16, token: &str, address: &str) -> Result<Vec<u8>, String> {
    let response = request_with_limit(
        host,
        port,
        token,
        "GET",
        &format!("/preview/{address}"),
        "",
        &[],
        8 * 1024 * 1024,
        &[],
    )?;
    if response.status == 200 {
        Ok(response.body)
    } else {
        Err(format!("Preview unavailable (HTTP {})", response.status))
    }
}

fn decode_preview(bytes: Vec<u8>) -> Result<DecodedPreview, String> {
    let decoded = image::load_from_memory(&bytes)
        .map_err(|error| format!("Preview image could not be decoded: {error}"))?;
    // The picker shows a 96×58 point card. Bound retained CPU/GPU texture size
    // while preserving each window's aspect ratio; decoding and downsampling
    // stay off the UI thread.
    let rgba = decoded.thumbnail(320, 200).to_rgba8();
    Ok(DecodedPreview {
        size: [rgba.width() as usize, rgba.height() as usize],
        rgba: rgba.into_raw(),
    })
}

fn shorten(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        text.to_owned()
    } else {
        format!(
            "{}...",
            text.chars()
                .take(max_chars.saturating_sub(3))
                .collect::<String>()
        )
    }
}

pub fn list_windows(host: &str, port: u16, token: &str) -> Result<Vec<WindowInfo>, String> {
    let (_, body) = request(host, port, token, "GET", "/windows", "", &[])?;
    let value: serde_json::Value = serde_json::from_slice(&body)
        .map_err(|error| format!("invalid window list response: {error}"))?;
    let entries = value
        .as_array()
        .ok_or_else(|| "window list response is not a JSON array".to_owned())?;
    entries.iter().map(WindowInfo::from_json).collect()
}

pub fn select_window(
    host: &str,
    port: u16,
    token: &str,
    address: Option<&str>,
    stable_id: Option<&str>,
) -> Result<(Option<WindowInfo>, u64), String> {
    let body =
        serde_json::to_vec(&serde_json::json!({ "address": address, "stable_id": stable_id }))
            .map_err(|error| format!("cannot encode selection: {error}"))?;
    let (_, body) = request(
        host,
        port,
        token,
        "POST",
        "/select",
        "application/json",
        &body,
    )?;
    let value: serde_json::Value = serde_json::from_slice(&body)
        .map_err(|error| format!("invalid select response: {error}"))?;
    let generation = value["generation"].as_u64().unwrap_or(0);
    let window = match value.get("selected") {
        None | Some(serde_json::Value::Null) => None,
        Some(window) => Some(WindowInfo::from_json(window)?),
    };
    Ok((window, generation))
}

pub(crate) struct HttpResponse {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl HttpResponse {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }
}

pub(crate) fn request(
    host: &str,
    port: u16,
    token: &str,
    method: &str,
    path: &str,
    content_type: &str,
    body: &[u8],
) -> Result<(u16, Vec<u8>), String> {
    let response = request_with_limit(
        host,
        port,
        token,
        method,
        path,
        content_type,
        body,
        2 * 1024 * 1024,
        &[],
    )?;
    if !(200..300).contains(&response.status) {
        let detail = String::from_utf8_lossy(&response.body).trim().to_owned();
        return Err(if detail.is_empty() {
            format!("window control request failed (HTTP {})", response.status)
        } else {
            format!(
                "window control request failed (HTTP {}): {detail}",
                response.status
            )
        });
    }
    Ok((response.status, response.body))
}

/// Make a bounded authenticated HTTP request. Extra headers are used for conditional
/// clipboard polls (If-None-Match); the response preserves headers and accepts 304.
pub(crate) fn request_with_limit(
    host: &str,
    port: u16,
    token: &str,
    method: &str,
    path: &str,
    content_type: &str,
    body: &[u8],
    max_response_bytes: usize,
    extra_headers: &[(&str, &str)],
) -> Result<HttpResponse, String> {
    if host.is_empty() || host.contains(['\r', '\n']) {
        return Err("invalid control host".into());
    }
    if token.contains(['\r', '\n'])
        || method.contains(['\r', '\n', ' '])
        || path.contains(['\r', '\n', ' '])
        || extra_headers
            .iter()
            .any(|(name, value)| name.contains(['\r', '\n', ':']) || value.contains(['\r', '\n']))
    {
        return Err("invalid HTTP request field".into());
    }
    let authority = crate::net::authority(host, port)?;
    let mut stream = crate::net::connect(host, port, Duration::from_secs(4))?;
    stream
        // /select may wait for the host to restart hypr-rdp and confirm its
        // listener is ready (up to 15 seconds).
        .set_read_timeout(Some(Duration::from_secs(20)))
        .map_err(|error| error.to_string())?;
    stream
        .set_write_timeout(Some(Duration::from_secs(10)))
        .map_err(|error| error.to_string())?;
    let content_type_header = if content_type.is_empty() {
        String::new()
    } else {
        format!("Content-Type: {content_type}\r\n")
    };
    let extra = extra_headers
        .iter()
        .map(|(name, value)| format!("{name}: {value}\r\n"))
        .collect::<String>();
    let headers = format!(
        "{method} {path} HTTP/1.1\r\nHost: {authority}\r\nAuthorization: Bearer {token}\r\n{content_type_header}{extra}Content-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    stream
        .write_all(headers.as_bytes())
        .and_then(|()| stream.write_all(body))
        .map_err(|error| format!("cannot send window control request: {error}"))?;

    const MAX_HEADER_BYTES: usize = 64 * 1024;
    let read_cap = max_response_bytes
        .checked_add(MAX_HEADER_BYTES)
        .and_then(|size| size.checked_add(4))
        .ok_or_else(|| "response limit is too large".to_owned())?;
    let mut raw = Vec::new();
    stream
        .take(read_cap as u64)
        .read_to_end(&mut raw)
        .map_err(|error| format!("cannot read window control response: {error}"))?;
    let separator = raw
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .ok_or_else(|| "malformed HTTP response from window control service".to_owned())?;
    if separator > MAX_HEADER_BYTES {
        return Err("HTTP response headers exceed 64 KiB".into());
    }
    let head = std::str::from_utf8(&raw[..separator])
        .map_err(|_| "invalid HTTP response headers".to_owned())?;
    let mut lines = head.lines();
    let status = lines
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|status| status.parse::<u16>().ok())
        .ok_or_else(|| "missing HTTP status code".to_owned())?;
    let mut response_headers = Vec::new();
    for line in lines {
        let (name, value) = line
            .split_once(':')
            .ok_or_else(|| "malformed HTTP response header".to_owned())?;
        response_headers.push((name.trim().to_owned(), value.trim().to_owned()));
    }
    let body = raw[separator + 4..].to_vec();
    if body.len() > max_response_bytes {
        return Err(format!("HTTP response exceeds {max_response_bytes} bytes"));
    }
    if let Some(length) = response_headers
        .iter()
        .find(|(name, _)| name.eq_ignore_ascii_case("content-length"))
        .map(|(_, value)| value.parse::<usize>())
    {
        let length = length.map_err(|_| "invalid Content-Length response header".to_owned())?;
        if length > max_response_bytes {
            return Err(format!("HTTP response exceeds {max_response_bytes} bytes"));
        }
        if length != body.len() && status != 304 {
            return Err("incomplete HTTP response body".into());
        }
    }
    Ok(HttpResponse {
        status,
        headers: response_headers,
        body,
    })
}

fn unit(value: f64) -> f64 {
    value.clamp(0.0, 1.0)
}

#[cfg(test)]
mod tests {
    use super::WindowPicker;

    #[test]
    fn preview_interval_defaults_and_clamps() {
        let mut picker = WindowPicker::default();
        assert_eq!(picker.preview_interval_secs(), 30);
        picker.set_preview_interval_secs(1);
        assert_eq!(picker.preview_interval_secs(), 5);
        picker.set_preview_interval_secs(900);
        assert_eq!(picker.preview_interval_secs(), 300);
        picker.set_preview_interval_secs(0);
        assert_eq!(picker.preview_interval_secs(), 0);
    }

    #[test]
    fn reset_clears_picker_state_and_preserves_interval() {
        let mut picker = WindowPicker::default();
        picker.set_preview_interval_secs(45);
        picker.windows.push(super::WindowInfo {
            address: "0x1".into(),
            stable_id: "test-window".into(),
            title: "Window".into(),
            class: "app".into(),
            monitor: "DP-1".into(),
            workspace: "1".into(),
            at: None,
            size: None,
        });
        picker.selected = Some("0x1".into());
        picker.selection_known = true;
        picker.loading = true;
        picker.message = "old connection".into();
        picker.reset();
        assert_eq!(picker.preview_interval_secs(), 45);
        assert!(picker.windows.is_empty());
        assert!(picker.selected.is_none());
        assert!(!picker.selection_known);
        assert!(!picker.loading);
        assert!(picker.message.is_empty());
        assert!(picker.worker_generation.is_none());
    }
}

fn display_value(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::String(text) => text.clone(),
        serde_json::Value::Null => String::new(),
        other => other.to_string(),
    }
}
