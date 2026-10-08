use super::{run_json, WindowView};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{
    env, fs,
    io::{self, Read, Write},
    net::{TcpListener, TcpStream},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc, Condvar, Mutex,
    },
    thread,
    time::Duration,
};

const PREVIEW_MAX: usize = 8 * 1024 * 1024;
const PREVIEW_TIMEOUT: Duration = Duration::from_secs(5);
const MAX_HEADERS: usize = 16 * 1024;
const CLIPBOARD_MAX: usize = 1024 * 1024;

#[derive(Serialize)]
pub(super) struct WindowInfo {
    address: String,
    stable_id: String,
    title: String,
    class: String,
    workspace: Value,
    monitor: String,
    fullscreen: bool,
    size: Value,
    at: Value,
}

pub(super) struct WindowControl {
    selected: Option<WindowView>,
    monitor: Option<String>,
    target_generation: Arc<AtomicU64>,
    restart_acks: RestartAcks,
    desktop: crate::desktop::DesktopState,
    instance_id: String,
}

#[derive(Clone, Default)]
pub(super) struct RestartAcks {
    result: Arc<(Mutex<Option<(u64, Result<(), String>)>>, Condvar)>,
}

impl RestartAcks {
    pub(super) fn complete(&self, generation: u64, result: Result<(), String>) {
        let (state, changed) = &*self.result;
        if let Ok(mut state) = state.lock() {
            if state
                .as_ref()
                .is_none_or(|(current, _)| generation >= *current)
            {
                *state = Some((generation, result));
            }
            changed.notify_all();
        }
    }

    fn wait(
        &self,
        generation: u64,
        timeout: Duration,
        stopping: &AtomicBool,
    ) -> Result<(), String> {
        let deadline = std::time::Instant::now() + timeout;
        let (state, changed) = &*self.result;
        let mut state = state
            .lock()
            .map_err(|_| "window capture supervisor unavailable".to_owned())?;
        loop {
            if let Some((completed_generation, result)) = state.as_ref() {
                if *completed_generation == generation {
                    return result.clone();
                }
                if *completed_generation > generation {
                    return Err("window selection was superseded".into());
                }
            }
            if stopping.load(Ordering::SeqCst) {
                return Err("host is stopping".into());
            }
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            if remaining.is_zero() {
                return Err("timed out waiting for the selected-window capture to start".into());
            }
            let (next_state, result) = changed
                .wait_timeout(state, remaining.min(Duration::from_millis(100)))
                .map_err(|_| "window capture supervisor unavailable".to_owned())?;
            state = next_state;
            if result.timed_out() && std::time::Instant::now() >= deadline {
                return Err("timed out waiting for the selected-window capture to start".into());
            }
        }
    }
}

impl WindowControl {
    pub(super) fn new(
        selected: Option<WindowView>,
        target_generation: Arc<AtomicU64>,
        restart_acks: RestartAcks,
        monitor: Option<String>,
    ) -> Self {
        let mut desktop = crate::desktop::DesktopState::default();
        desktop.select(
            selected.as_ref(),
            &run_json("hyprctl", &["-j", "clients"]).unwrap_or(Value::Null),
        );
        Self {
            selected,
            monitor,
            target_generation,
            restart_acks,
            desktop,
            instance_id: format!(
                "{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_nanos()
            ),
        }
    }

    pub(super) fn selected_target(&self) -> Option<WindowView> {
        self.selected.clone()
    }

    pub(super) fn selected_monitor(&self) -> Option<String> {
        self.monitor.clone()
    }

    fn select_monitor(&mut self, name: &str) -> Result<u64, ApiError> {
        let outputs =
            run_json("hyprctl", &["-j", "monitors"]).map_err(|e| ApiError::new(500, e))?;
        if !outputs
            .as_array()
            .is_some_and(|items| items.iter().any(|item| item["name"].as_str() == Some(name)))
        {
            return Err(ApiError::new(404, "monitor is not connected"));
        }
        self.monitor = Some(name.to_owned());
        self.selected = None;
        self.desktop.select(None, &Value::Null);
        Ok(self.target_generation.fetch_add(1, Ordering::SeqCst) + 1)
    }

    fn change_target(&mut self, target: Option<WindowView>) -> Option<u64> {
        if target.is_none() && self.selected.is_none() {
            return None;
        }
        self.selected = target;
        Some(self.target_generation.fetch_add(1, Ordering::SeqCst) + 1)
    }

    fn list(&self) -> Result<Vec<WindowInfo>, String> {
        let clients = run_json("hyprctl", &["-j", "clients"])?;
        let monitors = run_json("hyprctl", &["-j", "monitors"])?;
        let all_monitors = monitors.as_array().cloned().unwrap_or_default();
        Ok(clients
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|client| {
                let monitor_id = client.get("monitor").and_then(Value::as_i64)?;
                let monitor_name = all_monitors
                    .iter()
                    .find(|monitor| monitor.get("id").and_then(Value::as_i64) == Some(monitor_id))?
                    .get("name")
                    .and_then(Value::as_str)?
                    .to_owned();
                let fullscreen = client.get("fullscreen").is_some_and(|value| {
                    value
                        .as_bool()
                        .unwrap_or_else(|| value.as_i64().unwrap_or(0) > 0)
                });
                Some(WindowInfo {
                    address: client.get("address").and_then(Value::as_str)?.to_owned(),
                    stable_id: client.get("stableId").and_then(Value::as_str)?.to_owned(),
                    title: client
                        .get("title")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_owned(),
                    class: client
                        .get("class")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_owned(),
                    workspace: client.get("workspace").cloned().unwrap_or(Value::Null),
                    monitor: monitor_name,
                    fullscreen,
                    size: client.get("size").cloned().unwrap_or(Value::Null),
                    at: client.get("at").cloned().unwrap_or(Value::Null),
                })
            })
            .collect())
    }

    fn select(
        &mut self,
        address: Option<&str>,
        stable_id: Option<&str>,
    ) -> Result<(Option<WindowInfo>, Option<u64>), ApiError> {
        let Some(address) = address else {
            self.desktop.select(None, &Value::Null);
            let generation = self.change_target(None);
            return Ok((None, generation));
        };
        let next = WindowView::inspect(address).map_err(|message| {
            if message.starts_with("invalid") {
                ApiError::new(400, message)
            } else if message.starts_with("no Hyprland window found") {
                ApiError::new(404, message)
            } else {
                ApiError::new(500, message)
            }
        })?;
        let address = next.address.clone();
        if stable_id.is_some_and(|id| id != next.stable_id) {
            return Err(ApiError::new(
                409,
                "Window identity changed; refresh the window list",
            ));
        }
        let window = self
            .list()
            .map_err(|e| ApiError::new(500, e))?
            .into_iter()
            .find(|window| window.address == address)
            .ok_or_else(|| ApiError::new(404, "selected window closed"))?;
        // Re-requesting the current address is intentional: it can recover a
        // target whose child exited after the window itself was selected.
        self.desktop.select(
            Some(&next),
            &run_json("hyprctl", &["-j", "clients"]).unwrap_or(Value::Null),
        );
        let generation = self.change_target(Some(next)).unwrap();
        Ok((Some(window), Some(generation)))
    }
}

#[derive(Deserialize)]
struct SelectRequest {
    address: Option<String>,
    stable_id: Option<String>,
}

pub(crate) struct Request {
    pub(crate) method: String,
    pub(crate) path: String,
    pub(crate) headers: Vec<(String, String)>,
    pub(crate) body: Vec<u8>,
    pub(crate) content_length: u64,
}

#[derive(Debug)]
pub(crate) struct ApiError {
    pub(crate) status: u16,
    pub(crate) message: String,
}
impl ApiError {
    pub(crate) fn new(status: u16, message: impl Into<String>) -> Self {
        Self {
            status,
            message: message.into(),
        }
    }
}

impl Request {
    pub(crate) fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }
}

pub(super) fn run(
    listener: TcpListener,
    state: Arc<Mutex<WindowControl>>,
    token: String,
    stopping: Arc<AtomicBool>,
) {
    // Snapshots validate identity after window close/move. A separate worker
    // keeps this independent from slow previews and selection acknowledgements.
    let desktop_state = state.clone();
    let desktop_stopping = stopping.clone();
    thread::spawn(move || {
        while !desktop_stopping.load(Ordering::SeqCst) {
            let observed_generation = desktop_state
                .lock()
                .ok()
                .map(|state| state.target_generation.load(Ordering::SeqCst));
            if let (Ok(clients), Ok(monitors)) = (
                run_json("hyprctl", &["-j", "clients"]),
                run_json("hyprctl", &["-j", "monitors"]),
            ) {
                if let Ok(mut state) = desktop_state.lock() {
                    if observed_generation == Some(state.target_generation.load(Ordering::SeqCst)) {
                        if let Some(current) = state.selected.clone() {
                            if crate::desktop::find(&current, &clients).is_none()
                                && state.desktop.has_return()
                            {
                                if let Some(parent) =
                                    state.desktop.return_target(&clients, &monitors)
                                {
                                    state.change_target(Some(parent));
                                }
                            } else if let Some(updated) =
                                crate::desktop::refreshed(&current, &clients, &monitors)
                            {
                                if updated.output != current.output {
                                    state.change_target(Some(updated));
                                }
                            }
                        }
                    }
                }
            }
            thread::sleep(Duration::from_millis(400));
        }
    });
    use std::sync::atomic::AtomicUsize;
    const WORKERS: usize = 16;
    let active = Arc::new(AtomicUsize::new(0));
    let files = Arc::new(crate::files::TransferManager::default());
    let clipboard = Arc::new(crate::clipboard::ClipboardService::default());
    let terminal = Arc::new(crate::terminal::TerminalService::default());
    struct WorkerCount(Arc<AtomicUsize>);
    impl Drop for WorkerCount {
        fn drop(&mut self) {
            self.0.fetch_sub(1, Ordering::SeqCst);
        }
    }
    while !stopping.load(Ordering::SeqCst) {
        match listener.accept() {
            Ok((mut stream, _)) => {
                if active.fetch_add(1, Ordering::SeqCst) >= WORKERS {
                    active.fetch_sub(1, Ordering::SeqCst);
                    let _ = stream.set_write_timeout(Some(Duration::from_secs(1)));
                    let _ = send_response(
                        &mut stream,
                        503,
                        "application/json",
                        "{\"error\":\"control API busy\"}",
                    );
                    continue;
                }
                let guard = WorkerCount(active.clone());
                let state = state.clone();
                let token = token.clone();
                let stopping = stopping.clone();
                let files = files.clone();
                let clipboard = clipboard.clone();
                let terminal = terminal.clone();
                thread::spawn(move || {
                    let _guard = guard;
                    if let Err(error) = handle(
                        &mut stream,
                        &state,
                        &token,
                        &stopping,
                        &files,
                        &clipboard,
                        &terminal,
                    ) {
                        // A binary response already started must end by closing,
                        // never by appending a second HTTP response to its body.
                        if error.status != 0 {
                            let _ = send_response(
                                &mut stream,
                                error.status,
                                "application/json",
                                &json!({"error": error.message}).to_string(),
                            );
                        }
                    }
                });
            }
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(30))
            }
            Err(error) => {
                eprintln!("windowlink-host: control listener stopped: {error}");
                break;
            }
        }
    }
}

fn handle(
    stream: &mut TcpStream,
    state: &Arc<Mutex<WindowControl>>,
    token: &str,
    stopping: &AtomicBool,
    files: &crate::files::TransferManager,
    clipboard: &crate::clipboard::ClipboardService,
    terminal: &crate::terminal::TerminalService,
) -> Result<(), ApiError> {
    stream
        .set_read_timeout(Some(Duration::from_secs(15)))
        .map_err(|e| ApiError::new(500, e.to_string()))?;
    stream
        .set_write_timeout(Some(Duration::from_secs(5)))
        .map_err(|e| ApiError::new(500, e.to_string()))?;
    let mut request = read_request(stream)?;
    let authorization = request
        .headers
        .iter()
        .find(|(name, _)| name.eq_ignore_ascii_case("authorization"))
        .map(|(_, value)| value.as_str())
        .unwrap_or("");
    let supplied = authorization
        .split_once(' ')
        .filter(|(scheme, _)| scheme.eq_ignore_ascii_case("Bearer"))
        .map(|(_, value)| value.trim())
        .unwrap_or("");
    if !constant_time_eq(supplied.as_bytes(), token.as_bytes()) {
        return send_response(
            stream,
            401,
            "application/json",
            &json!({"error":"unauthorized"}).to_string(),
        )
        .map_err(|e| ApiError::new(500, e.to_string()));
    }

    let streaming = (request.method == "PATCH" && request.path.starts_with("/transfers/"))
        || (request.method == "POST" && request.path.starts_with("/files/"));
    if !streaming {
        read_body(stream, &mut request)?;
    } else {
        stream
            .set_read_timeout(Some(Duration::from_secs(1)))
            .map_err(crate::files::io_error)?;
    }
    if let Some(result) = files.route(&request, stream, stopping) {
        return result;
    }
    if let Some(result) = crate::files::fs_route(&request, stream) {
        return result;
    }
    if let Some(result) = clipboard.route(&request, stream) {
        return result;
    }
    if let Some(result) = terminal.route(&request, stream) {
        return result;
    }

    match (request.method.as_str(), request.path.as_str()) {
        ("GET", "/desktop-state") => {
            let clients =
                run_json("hyprctl", &["-j", "clients"]).map_err(|e| ApiError::new(500, e))?;
            let monitors =
                run_json("hyprctl", &["-j", "monitors"]).map_err(|e| ApiError::new(500, e))?;
            let control = state
                .lock()
                .map_err(|_| ApiError::new(500, "state unavailable"))?;
            let generation = control.target_generation.load(Ordering::SeqCst);
            let mut value = control.desktop.describe(
                control.selected.as_ref(),
                control.monitor.as_deref(),
                &clients,
                &monitors,
            );
            value["generation"] = json!(generation);
            value["instance_id"] = json!(control.instance_id);
            value["requested_generation"] = json!(generation);
            let result = control
                .restart_acks
                .result
                .0
                .lock()
                .map_err(|_| ApiError::new(500, "restart state unavailable"))?;
            let completed = result
                .as_ref()
                .filter(|(completed, _)| *completed == generation);
            value["active_generation"] = result
                .as_ref()
                .filter(|(_, result)| result.is_ok())
                .map_or(Value::Null, |(generation, _)| json!(generation));
            // This describes the restart acknowledgement only; the phone must
            // still wait for a decoded frame before enabling input.
            value["status"] = json!(match completed {
                Some((_, Ok(()))) => "ready",
                Some((_, Err(_))) => "error",
                None if generation == 0 => "ready",
                None => "switching",
            });
            value["error"] = completed
                .and_then(|(_, result)| result.as_ref().err())
                .map_or(Value::Null, |e| json!(e));
            value["capture_ready"] = Value::Null;
            if control
                .selected
                .as_ref()
                .is_some_and(|target| crate::desktop::find(target, &clients).is_none())
            {
                value["status"] = json!("error");
                value["error"] =
                    json!("Selected window closed. Choose another window or show a monitor.");
            }
            let output = control
                .selected
                .as_ref()
                .map(|target| target.output.as_str())
                .or(control.monitor.as_deref());
            if output.is_some_and(|output| {
                !monitors.as_array().is_some_and(|items| {
                    items.iter().any(|m| {
                        m["name"].as_str() == Some(output) && m["disabled"].as_bool() != Some(true)
                    })
                })
            }) {
                value["status"] = json!("error");
                value["error"] =
                    json!("Selected monitor disconnected. Choose an available monitor.");
            }
            send_response(stream, 200, "application/json", &value.to_string())
                .map_err(|e| ApiError::new(500, e.to_string()))
        }
        ("POST", "/target/related") | ("POST", "/target/return") => {
            let clients =
                run_json("hyprctl", &["-j", "clients"]).map_err(|e| ApiError::new(500, e))?;
            let monitors =
                run_json("hyprctl", &["-j", "monitors"]).map_err(|e| ApiError::new(500, e))?;
            let (generation, acks, target) = {
                let mut control = state
                    .lock()
                    .map_err(|_| ApiError::new(500, "state unavailable"))?;
                let next = if request.path == "/target/return" {
                    control
                        .desktop
                        .return_target(&clients, &monitors)
                        .ok_or_else(|| ApiError::new(404, "No saved window is still available"))?
                } else {
                    let value: Value = serde_json::from_slice(&request.body)
                        .map_err(|e| ApiError::new(400, e.to_string()))?;
                    let address = value["address"]
                        .as_str()
                        .ok_or_else(|| ApiError::new(400, "address required"))?;
                    let stable_id = value["stable_id"]
                        .as_str()
                        .ok_or_else(|| ApiError::new(400, "stable_id required"))?;
                    let next = crate::desktop::refreshed(
                        &WindowView {
                            address: address.into(),
                            stable_id: stable_id.into(),
                            output: String::new(),
                        },
                        &clients,
                        &monitors,
                    )
                    .ok_or_else(|| {
                        ApiError::new(404, "Related window closed or identity changed")
                    })?;
                    let current = control
                        .selected
                        .clone()
                        .ok_or_else(|| ApiError::new(409, "Select an app window first"))?;
                    if current.stable_id == next.stable_id {
                        return Err(ApiError::new(400, "Window is already selected"));
                    }
                    control.desktop.open_related(&current);
                    next
                };
                let target = json!({"kind":"window","address":next.address,"stable_id":next.stable_id,"output":next.output});
                let generation = control.change_target(Some(next)).unwrap();
                (generation, control.restart_acks.clone(), target)
            };
            acks.wait(generation, Duration::from_secs(15), stopping)
                .map_err(|e| ApiError::new(503, e))?;
            send_response(
                stream,
                200,
                "application/json",
                &json!({"generation":generation,"target":target}).to_string(),
            )
            .map_err(|e| ApiError::new(500, e.to_string()))
        }
        ("GET", "/cursor") => {
            let (target, monitor, generation) = {
                let control = state
                    .lock()
                    .map_err(|_| ApiError::new(500, "state unavailable"))?;
                (
                    control.selected_target(),
                    control.selected_monitor(),
                    control.target_generation.load(Ordering::SeqCst),
                )
            };
            let cursor =
                run_json("hyprctl", &["-j", "cursorpos"]).map_err(|e| ApiError::new(500, e))?;
            let (at, size) = if let Some(target) = target {
                let clients =
                    run_json("hyprctl", &["-j", "clients"]).map_err(|e| ApiError::new(500, e))?;
                let window = clients
                    .as_array()
                    .and_then(|a| {
                        a.iter().find(|w| {
                            w["address"].as_str() == Some(&target.address)
                                && w["stableId"].as_str() == Some(&target.stable_id)
                        })
                    })
                    .ok_or_else(|| ApiError::new(404, "capture target closed"))?;
                (
                    [
                        window["at"][0].as_f64().unwrap_or(0.0),
                        window["at"][1].as_f64().unwrap_or(0.0),
                    ],
                    [
                        window["size"][0].as_f64().unwrap_or(1.0),
                        window["size"][1].as_f64().unwrap_or(1.0),
                    ],
                )
            } else {
                let outputs =
                    run_json("hyprctl", &["-j", "monitors"]).map_err(|e| ApiError::new(500, e))?;
                let outputs = outputs
                    .as_array()
                    .ok_or_else(|| ApiError::new(500, "invalid outputs"))?;
                let output = outputs
                    .iter()
                    .find(|m| {
                        monitor
                            .as_deref()
                            .is_some_and(|name| m["name"].as_str() == Some(name))
                    })
                    .or_else(|| {
                        outputs
                            .iter()
                            .find(|m| m["focused"].as_bool() == Some(true))
                    })
                    .or_else(|| outputs.first())
                    .ok_or_else(|| ApiError::new(404, "no monitor"))?;
                let scale = output["scale"].as_f64().unwrap_or(1.0).max(0.01);
                let mut size = [
                    output["width"].as_f64().unwrap_or(1.0) / scale,
                    output["height"].as_f64().unwrap_or(1.0) / scale,
                ];
                if output["transform"].as_u64().unwrap_or(0) % 2 == 1 {
                    size.swap(0, 1);
                }
                (
                    [
                        output["x"].as_f64().unwrap_or(0.0),
                        output["y"].as_f64().unwrap_or(0.0),
                    ],
                    size,
                )
            };
            let x = (cursor["x"].as_f64().unwrap_or(0.0) - at[0]) / size[0].max(1.0);
            let y = (cursor["y"].as_f64().unwrap_or(0.0) - at[1]) / size[1].max(1.0);
            send_response(stream, 200, "application/json", &json!({"x":x,"y":y,"visible":(0.0..1.0).contains(&x)&&(0.0..1.0).contains(&y),"generation":generation,"logical_size":size}).to_string())
                .map_err(|e| ApiError::new(500, e.to_string()))
        }
        ("GET", "/outputs") => {
            let outputs =
                run_json("hyprctl", &["-j", "monitors"]).map_err(|e| ApiError::new(500, e))?;
            send_response(stream, 200, "application/json", &outputs.to_string())
                .map_err(|e| ApiError::new(500, e.to_string()))
        }
        ("POST", "/monitor") => {
            let value: Value = serde_json::from_slice(&request.body)
                .map_err(|e| ApiError::new(400, e.to_string()))?;
            let name = value["name"]
                .as_str()
                .ok_or_else(|| ApiError::new(400, "monitor name required"))?;
            let (generation, acks) = {
                let mut control = state
                    .lock()
                    .map_err(|_| ApiError::new(500, "state unavailable"))?;
                (control.select_monitor(name)?, control.restart_acks.clone())
            };
            acks.wait(generation, Duration::from_secs(15), stopping)
                .map_err(|e| ApiError::new(503, e))?;
            send_response(
                stream,
                200,
                "application/json",
                &json!({"monitor":name,"generation":generation}).to_string(),
            )
            .map_err(|e| ApiError::new(500, e.to_string()))
        }
        ("GET", "/windows") => {
            let windows = state
                .lock()
                .map_err(|_| ApiError::new(500, "window state unavailable"))?
                .list()
                .map_err(|e| ApiError::new(500, e))?;
            send_response(
                stream,
                200,
                "application/json",
                &serde_json::to_string(&windows).unwrap(),
            )
            .map_err(|e| ApiError::new(500, e.to_string()))
        }
        ("POST", "/select") => {
            let value: Value = serde_json::from_slice(&request.body)
                .map_err(|e| ApiError::new(400, format!("invalid JSON: {e}")))?;
            if !value
                .as_object()
                .is_some_and(|object| object.contains_key("address"))
            {
                return Err(ApiError::new(400, "request must contain an address field"));
            }
            let selection: SelectRequest =
                serde_json::from_value(value).map_err(|e| ApiError::new(400, e.to_string()))?;
            let (selected, generation, restart_acks, requested_generation) = {
                let mut state = state
                    .lock()
                    .map_err(|_| ApiError::new(500, "window state unavailable"))?;
                let (selected, generation) =
                    state.select(selection.address.as_deref(), selection.stable_id.as_deref())?;
                (
                    selected,
                    generation,
                    state.restart_acks.clone(),
                    state.target_generation.load(Ordering::SeqCst),
                )
            };
            if let Some(generation) = generation {
                restart_acks
                    .wait(generation, Duration::from_secs(15), stopping)
                    .map_err(|message| ApiError::new(503, message))?;
            }
            send_response(
                stream,
                200,
                "application/json",
                &json!({"selected":selected,"generation":requested_generation}).to_string(),
            )
            .map_err(|e| ApiError::new(500, e.to_string()))
        }
        ("GET", "/clipboard") => {
            let text = crate::clipboard::read_text()?;
            let etag = clipboard_etag(text.as_bytes());
            let requested = request
                .headers
                .iter()
                .find(|(name, _)| name.eq_ignore_ascii_case("if-none-match"))
                .map(|(_, value)| value.trim());
            let result = if requested == Some(etag.as_str()) {
                send_etag_response(stream, 304, "text/plain; charset=utf-8", &[], &etag)
            } else {
                send_etag_response(
                    stream,
                    200,
                    "text/plain; charset=utf-8",
                    text.as_bytes(),
                    &etag,
                )
            };
            result.map_err(|e| ApiError::new(500, e.to_string()))
        }
        ("POST", "/clipboard") => {
            if request.body.len() > crate::clipboard::text_max() {
                return Err(ApiError::new(413, "clipboard body exceeds 1 MiB"));
            }
            let content_type = request
                .headers
                .iter()
                .find(|(name, _)| name.eq_ignore_ascii_case("content-type"))
                .map(|(_, value)| value.to_ascii_lowercase())
                .unwrap_or_default();
            if !content_type.starts_with("text/plain") {
                return Err(ApiError::new(
                    415,
                    "clipboard body must be text/plain UTF-8",
                ));
            }
            let text = std::str::from_utf8(&request.body)
                .map_err(|_| ApiError::new(400, "clipboard body is not valid UTF-8"))?;
            crate::clipboard::write_text(text)?;
            send_response(stream, 204, "text/plain; charset=utf-8", "")
                .map_err(|e| ApiError::new(500, e.to_string()))
        }
        ("GET", "/files") => {
            let files = list_files()?;
            send_response(
                stream,
                200,
                "application/json",
                &serde_json::to_string(&files).unwrap(),
            )
            .map_err(|e| ApiError::new(500, e.to_string()))
        }
        ("GET", path) if path.starts_with("/preview/") => {
            let address = &path["/preview/".len()..];
            let address = normalize_window_address(address)?;
            let clients = run_json("hyprctl", &["-j", "clients"])
                .map_err(|error| ApiError::new(500, error))?;
            let (stable_id, window_size) = clients
                .as_array()
                .into_iter()
                .flatten()
                .find(|client| {
                    client
                        .get("address")
                        .and_then(Value::as_str)
                        .is_some_and(|candidate| candidate.eq_ignore_ascii_case(&address))
                })
                .and_then(|client| {
                    let stable_id = client.get("stableId")?.as_str()?;
                    let size = client.get("size")?.as_array()?;
                    let width = size.first()?.as_u64()?.try_into().ok()?;
                    let height = size.get(1)?.as_u64()?.try_into().ok()?;
                    (preview_scale(width, height).is_some())
                        .then(|| (stable_id.to_owned(), [width, height]))
                })
                .ok_or_else(|| {
                    ApiError::new(
                        404,
                        "window not found or does not expose a stable capture id and valid size",
                    )
                })?;
            let png = capture_preview(&stable_id, window_size)?;
            send_bytes(stream, 200, "image/png", &png)
                .map_err(|e| ApiError::new(500, e.to_string()))
        }
        ("GET" | "POST", path) if path.starts_with("/files/") => {
            let name = decode_file_name(&path["/files/".len()..])?;
            let directory = file_directory()?;
            crate::files::legacy_route(&request, stream, &directory, &name, stopping)
        }
        _ => Err(ApiError::new(404, "unknown control API route")),
    }
}

fn normalize_window_address(address: &str) -> Result<String, ApiError> {
    let normalized = address.strip_prefix("0x").unwrap_or(address);
    if normalized.is_empty() || !normalized.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(ApiError::new(400, "window address must be hexadecimal"));
    }
    Ok(format!("0x{}", normalized.to_ascii_lowercase()))
}

const PREVIEW_MAX_DIMENSION: u32 = 384;

fn preview_scale(width: u32, height: u32) -> Option<f64> {
    if width == 0 || height == 0 {
        return None;
    }
    Some((PREVIEW_MAX_DIMENSION as f64 / width.max(height) as f64).min(1.0))
}

fn capture_preview(stable_id: &str, size: [u32; 2]) -> Result<Vec<u8>, ApiError> {
    let scale = preview_scale(size[0], size[1])
        .ok_or_else(|| ApiError::new(500, "window has invalid size for preview"))?;
    // grim's scale factor preserves the capture aspect ratio while keeping
    // large window previews small enough for the overview list.
    let scale = format!("{scale:.8}");
    let mut child = Command::new("grim")
        .args(["-s", &scale, "-t", "png", "-T", stable_id, "-"])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|error| ApiError::new(500, format!("cannot start grim: {error}")))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| ApiError::new(500, "grim has no stdout"))?;
    let reader = thread::spawn(move || {
        let mut bytes = Vec::new();
        stdout
            .take((PREVIEW_MAX + 1) as u64)
            .read_to_end(&mut bytes)
            .map(|_| bytes)
    });
    let deadline = std::time::Instant::now() + PREVIEW_TIMEOUT;
    let status = loop {
        if std::time::Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            let _ = reader.join();
            return Err(ApiError::new(504, "window preview capture timed out"));
        }
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => thread::sleep(Duration::from_millis(20)),
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                let _ = reader.join();
                return Err(ApiError::new(
                    500,
                    format!("could not wait for grim: {error}"),
                ));
            }
        }
    };
    let bytes = reader
        .join()
        .map_err(|_| ApiError::new(500, "grim output reader failed"))?
        .map_err(|error| ApiError::new(500, format!("could not read grim output: {error}")))?;
    if bytes.len() > PREVIEW_MAX {
        return Err(ApiError::new(413, "window preview exceeds 8 MiB"));
    }
    if !status.success() {
        return Err(ApiError::new(
            500,
            "grim could not capture the requested window",
        ));
    }
    if !bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        return Err(ApiError::new(500, "grim returned invalid PNG data"));
    }
    Ok(bytes)
}

#[cfg(test)]
mod preview_scale_tests {
    use super::{preview_scale, PREVIEW_MAX_DIMENSION};

    #[test]
    fn large_window_scales_long_edge_to_preview_bound_and_keeps_ratio() {
        let scale = preview_scale(1920, 1080).unwrap();
        assert!((1920.0 * scale - PREVIEW_MAX_DIMENSION as f64).abs() < 0.001);
        assert!((1080.0 * scale - 216.0).abs() < 0.001);
    }

    #[test]
    fn normal_small_window_does_not_upscale() {
        assert_eq!(preview_scale(320, 200), Some(1.0));
    }

    #[test]
    fn zero_sized_client_window_is_rejected() {
        assert_eq!(preview_scale(0, 720), None);
        assert_eq!(preview_scale(1280, 0), None);
    }
}

#[cfg(test)]
mod restart_ack_tests {
    use super::{RestartAcks, WindowControl};
    use crate::WindowView;
    use std::{
        sync::{atomic::AtomicBool, Arc},
        thread,
        time::Duration,
    };

    #[test]
    fn selection_waits_for_its_own_applied_generation() {
        let acks = RestartAcks::default();
        let stopping = Arc::new(AtomicBool::new(false));
        acks.complete(1, Ok(()));

        let waiter_acks = acks.clone();
        let waiter_stopping = Arc::clone(&stopping);
        let (ready_tx, ready_rx) = std::sync::mpsc::channel();
        let waiter = thread::spawn(move || {
            ready_tx.send(()).unwrap();
            waiter_acks.wait(2, Duration::from_secs(1), &waiter_stopping)
        });
        ready_rx.recv().unwrap();
        acks.complete(2, Ok(()));
        acks.complete(1, Err("stale generation".into()));
        assert_eq!(waiter.join().unwrap(), Ok(()));
        assert_eq!(acks.wait(2, Duration::from_millis(1), &stopping), Ok(()));
    }

    #[test]
    fn child_start_failure_is_returned_to_the_selection_request() {
        let acks = RestartAcks::default();
        let stopping = AtomicBool::new(false);
        acks.complete(7, Err("capture did not start".into()));
        assert_eq!(
            acks.wait(7, Duration::from_millis(1), &stopping),
            Err("capture did not start".into())
        );
    }

    #[test]
    fn selecting_same_target_requests_a_fresh_child_and_desktop_clears_it() {
        let generation = Arc::new(std::sync::atomic::AtomicU64::new(0));
        let mut state =
            WindowControl::new(None, Arc::clone(&generation), RestartAcks::default(), None);
        let target = WindowView {
            address: "0xabc".into(),
            stable_id: "toplevel-abc".into(),
            output: "DP-1".into(),
        };

        assert_eq!(state.change_target(Some(target.clone())), Some(1));
        assert_eq!(state.change_target(Some(target)), Some(2));
        assert_eq!(state.change_target(None), Some(3));
        assert_eq!(state.change_target(None), None);
        assert_eq!(generation.load(std::sync::atomic::Ordering::SeqCst), 3);
        assert!(state.selected_target().is_none());
    }
}

fn read_request(stream: &mut TcpStream) -> Result<Request, ApiError> {
    let mut input = Vec::new();
    let mut chunk = [0u8; 4096];
    let position = loop {
        let count = stream
            .read(&mut chunk)
            .map_err(|e| ApiError::new(400, format!("could not read request: {e}")))?;
        if count == 0 {
            return Err(ApiError::new(400, "incomplete HTTP request"));
        }
        input.extend_from_slice(&chunk[..count]);
        if let Some(position) = input.windows(4).position(|bytes| bytes == b"\r\n\r\n") {
            if position > MAX_HEADERS {
                return Err(ApiError::new(431, "HTTP headers too large"));
            }
            break position;
        }
        if input.len() > MAX_HEADERS {
            return Err(ApiError::new(431, "HTTP headers too large"));
        }
    };
    let header = std::str::from_utf8(&input[..position])
        .map_err(|_| ApiError::new(400, "HTTP headers must be UTF-8"))?;
    let mut lines = header.lines();
    let mut words = lines
        .next()
        .ok_or_else(|| ApiError::new(400, "missing request line"))?
        .split_whitespace();
    let method = words
        .next()
        .ok_or_else(|| ApiError::new(400, "missing HTTP method"))?
        .to_owned();
    let path = words
        .next()
        .ok_or_else(|| ApiError::new(400, "missing HTTP path"))?
        .to_owned();
    if !words
        .next()
        .is_some_and(|v| v == "HTTP/1.1" || v == "HTTP/1.0")
        || words.next().is_some()
    {
        return Err(ApiError::new(400, "invalid HTTP version"));
    }
    let mut headers = Vec::new();
    let mut content_length = None;
    for line in lines {
        let (name, value) = line
            .split_once(':')
            .ok_or_else(|| ApiError::new(400, "malformed HTTP header"))?;
        if name.eq_ignore_ascii_case("content-length") {
            if content_length.is_some() {
                return Err(ApiError::new(400, "duplicate Content-Length"));
            }
            content_length = Some(
                value
                    .trim()
                    .parse::<u64>()
                    .map_err(|_| ApiError::new(400, "invalid Content-Length"))?,
            );
        }
        if name.eq_ignore_ascii_case("transfer-encoding")
            && !value.trim().eq_ignore_ascii_case("identity")
        {
            return Err(ApiError::new(
                400,
                "chunked requests are not supported; use Content-Length",
            ));
        }
        headers.push((name.trim().to_owned(), value.trim().to_owned()));
    }
    let content_length = content_length.unwrap_or(0);
    // Only the small read-ahead prefix is retained before authentication.
    let prefix = &input[position + 4..];
    let prefix_length = prefix
        .len()
        .min(content_length.min(usize::MAX as u64) as usize);
    Ok(Request {
        method,
        path,
        headers,
        content_length,
        body: prefix[..prefix_length].to_vec(),
    })
}

fn read_body(stream: &mut TcpStream, request: &mut Request) -> Result<(), ApiError> {
    let path = request.path.split('?').next().unwrap_or("");
    let mime = request
        .header("content-type")
        .unwrap_or("")
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    let maximum = if path == "/clipboard/content" {
        if mime == "text/plain" {
            crate::clipboard::text_max()
        } else {
            crate::clipboard::image_max()
        }
    } else if path == "/clipboard" {
        crate::clipboard::text_max()
    } else {
        CLIPBOARD_MAX
    };
    if request.content_length > maximum as u64 {
        return Err(ApiError::new(413, "request body exceeds route size limit"));
    }
    let remaining = request.content_length as usize - request.body.len();
    let start = request.body.len();
    request.body.resize(start + remaining, 0);
    stream
        .read_exact(&mut request.body[start..])
        .map_err(|e| ApiError::new(400, format!("incomplete request body: {e}")))
}

fn send_response(
    stream: &mut TcpStream,
    status: u16,
    content_type: &str,
    body: &str,
) -> io::Result<()> {
    send_bytes(stream, status, content_type, body.as_bytes())
}

pub(crate) fn send_bytes(
    stream: &mut TcpStream,
    status: u16,
    content_type: &str,
    body: &[u8],
) -> io::Result<()> {
    send_etag_response(stream, status, content_type, body, "")
}

pub(crate) fn send_etag_response(
    stream: &mut TcpStream,
    status: u16,
    content_type: &str,
    body: &[u8],
    etag: &str,
) -> io::Result<()> {
    let reason = match status {
        200 => "OK",
        201 => "Created",
        204 => "No Content",
        304 => "Not Modified",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        408 => "Request Timeout",
        404 => "Not Found",
        409 => "Conflict",
        412 => "Precondition Failed",
        413 => "Payload Too Large",
        416 => "Range Not Satisfiable",
        429 => "Too Many Requests",
        503 => "Service Unavailable",
        504 => "Gateway Timeout",
        507 => "Insufficient Storage",
        415 => "Unsupported Media Type",
        431 => "Request Header Fields Too Large",
        _ => "Internal Server Error",
    };
    let etag_header = if etag.is_empty() {
        String::new()
    } else {
        format!("ETag: {etag}\r\n")
    };
    write!(
        stream,
        "HTTP/1.1 {status} {reason}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\n{etag_header}Connection: close\r\nCache-Control: no-store\r\n\r\n",
        body.len()
    )?;
    stream.write_all(body)?;
    stream.flush()
}

fn clipboard_etag(bytes: &[u8]) -> String {
    let mut hash = 0xcbf29ce484222325u64;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    format!("\"{hash:016x}\"")
}

#[derive(Serialize)]
struct FileInfo {
    name: String,
    size: u64,
    modified_unix: Option<u64>,
}

fn file_directory() -> Result<PathBuf, ApiError> {
    let home = env::var_os("HOME").ok_or_else(|| ApiError::new(500, "HOME is not set"))?;
    let directory = PathBuf::from(home).join("Downloads").join("WindowLink");
    fs::create_dir_all(&directory)
        .map_err(|e| ApiError::new(500, format!("cannot create transfer directory: {e}")))?;
    let canonical = fs::canonicalize(&directory).map_err(|e| ApiError::new(500, e.to_string()))?;
    let expected_parent =
        fs::canonicalize(Path::new(&env::var_os("HOME").unwrap()).join("Downloads"))
            .map_err(|e| ApiError::new(500, e.to_string()))?;
    if canonical.parent() != Some(expected_parent.as_path()) {
        return Err(ApiError::new(
            500,
            "transfer directory must remain directly under ~/Downloads",
        ));
    }
    Ok(canonical)
}

fn list_files() -> Result<Vec<FileInfo>, ApiError> {
    let directory = file_directory()?;
    let mut items = Vec::new();
    for entry in fs::read_dir(directory).map_err(|e| ApiError::new(500, e.to_string()))? {
        let entry = entry.map_err(|e| ApiError::new(500, e.to_string()))?;
        let metadata = entry
            .file_type()
            .map_err(|e| ApiError::new(500, e.to_string()))?;
        if !metadata.is_file() {
            continue;
        }
        let metadata = entry
            .metadata()
            .map_err(|e| ApiError::new(500, e.to_string()))?;
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        let modified_unix = metadata
            .modified()
            .ok()
            .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|duration| duration.as_secs());
        items.push(FileInfo {
            name,
            size: metadata.len(),
            modified_unix,
        });
    }
    items.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
    Ok(items)
}

fn decode_file_name(encoded: &str) -> Result<String, ApiError> {
    let bytes = encoded.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' {
            if index + 2 >= bytes.len() {
                return Err(ApiError::new(400, "malformed percent-encoding"));
            }
            let hex = std::str::from_utf8(&bytes[index + 1..index + 3]).unwrap_or("");
            decoded.push(
                u8::from_str_radix(hex, 16)
                    .map_err(|_| ApiError::new(400, "malformed percent-encoding"))?,
            );
            index += 3;
        } else {
            decoded.push(bytes[index]);
            index += 1;
        }
    }
    let name =
        String::from_utf8(decoded).map_err(|_| ApiError::new(400, "filename must be UTF-8"))?;
    if name.is_empty()
        || name == "."
        || name == ".."
        || name.contains(['/', '\\', '\0'])
        || name.chars().any(char::is_control)
    {
        return Err(ApiError::new(400, "filename must be one safe path segment"));
    }
    Ok(name)
}

fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
    let mut difference = left.len() ^ right.len();
    for index in 0..left.len().max(right.len()) {
        difference |= (left.get(index).copied().unwrap_or(0)
            ^ right.get(index).copied().unwrap_or(0)) as usize;
    }
    difference == 0
}

#[cfg(test)]
mod streaming_http_tests {
    use super::*;
    use std::net::Shutdown;

    #[test]
    fn unauthorized_large_body_is_rejected_before_reading_it() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let state = Arc::new(Mutex::new(WindowControl::new(
                None,
                Arc::new(AtomicU64::new(0)),
                RestartAcks::default(),
                None,
            )));
            handle(
                &mut stream,
                &state,
                "test-token",
                &AtomicBool::new(false),
                &crate::files::TransferManager::default(),
                &crate::clipboard::ClipboardService::default(),
                &crate::terminal::TerminalService::default(),
            )
            .unwrap();
        });
        let mut client = TcpStream::connect(address).unwrap();
        client
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        client
            .write_all(b"PATCH /transfers/test HTTP/1.1\r\nContent-Length: 1099511627776\r\n\r\n")
            .unwrap();
        let mut response = String::new();
        client.read_to_string(&mut response).unwrap();
        assert!(response.starts_with("HTTP/1.1 401"));
        server.join().unwrap();
    }

    #[test]
    fn headers_only_parser_retains_small_prefix_and_rejects_duplicate_lengths() {
        for (header, expected) in [
            (
                "POST /files/test HTTP/1.1\r\nContent-Length: 1099511627776\r\n\r\nabc",
                true,
            ),
            (
                "POST /files/test HTTP/1.1\r\nContent-Length: 2\r\nContent-Length: 3\r\n\r\n",
                false,
            ),
            (
                "POST /files/test HTTP/1.1\r\nTransfer-Encoding: chunked\r\n\r\n",
                false,
            ),
        ] {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let address = listener.local_addr().unwrap();
            let server = thread::spawn(move || {
                let (mut stream, _) = listener.accept().unwrap();
                read_request(&mut stream)
            });
            let mut client = TcpStream::connect(address).unwrap();
            client.write_all(header.as_bytes()).unwrap();
            client.shutdown(Shutdown::Write).unwrap();
            let result = server.join().unwrap();
            assert_eq!(result.is_ok(), expected);
            if let Ok(request) = result {
                assert_eq!(request.content_length, 1099511627776);
                assert_eq!(request.body, b"abc");
            }
        }
    }
}
