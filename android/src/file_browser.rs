//! Filesystem browser and a bounded, connection-scoped transfer queue.
use crate::icons::{self, Icon};
use eframe::egui;
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, VecDeque},
    io::{BufRead, BufReader, Read, Write},
    net::TcpStream,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc,
    },
    thread,
    time::Duration,
};

const BUFFER: usize = 64 * 1024;
static NEXT_JOB: AtomicU64 = AtomicU64::new(1);
#[derive(Clone, PartialEq, Eq)]
struct Connection {
    host: String,
    port: u16,
    token: String,
}
impl Connection {
    fn connect(
        &self,
        method: &str,
        path: &str,
        mime: &str,
        length: u64,
    ) -> Result<TcpStream, String> {
        if self.host.contains(['\r', '\n'])
            || self.token.contains(['\r', '\n'])
            || path.contains(['\r', '\n', ' '])
        {
            return Err("Invalid control request".into());
        }
        let authority = crate::net::authority(&self.host, self.port)?;
        let mut stream = crate::net::connect(&self.host, self.port, Duration::from_secs(4))?;
        stream
            .set_read_timeout(Some(Duration::from_secs(10)))
            .map_err(|e| e.to_string())?;
        stream
            .set_write_timeout(Some(Duration::from_secs(5)))
            .map_err(|e| e.to_string())?;
        write!(stream,"{method} {path} HTTP/1.1\r\nHost: {authority}\r\nAuthorization: Bearer {}\r\nContent-Type: {mime}\r\nContent-Length: {length}\r\nConnection: close\r\n\r\n",self.token).map_err(|e|e.to_string())?;
        Ok(stream)
    }
    fn json(&self, method: &str, path: &str, value: Option<Value>) -> Result<Value, String> {
        let body = value
            .map(|v| v.to_string().into_bytes())
            .unwrap_or_default();
        let mut stream = self.connect(method, path, "application/json", body.len() as u64)?;
        stream.write_all(&body).map_err(|e| e.to_string())?;
        let mut response = Response::new(stream)?;
        let bytes = response.bytes(8 * 1024 * 1024)?;
        if !(200..300).contains(&response.status) {
            return Err(error_message(response.status, &bytes));
        }
        if bytes.is_empty() {
            Ok(Value::Null)
        } else {
            serde_json::from_slice(&bytes).map_err(|e| e.to_string())
        }
    }
}
struct Response {
    status: u16,
    remaining: u64,
    reader: BufReader<TcpStream>,
}
impl Response {
    fn new(stream: TcpStream) -> Result<Self, String> {
        let mut reader = BufReader::with_capacity(BUFFER, stream);
        let mut head = String::new();
        let mut total = 0;
        let mut status = None;
        let mut length = None;
        loop {
            let mut line = String::new();
            let n = std::io::Read::by_ref(&mut reader)
                .take((16 * 1024 - total) as u64)
                .read_line(&mut line)
                .map_err(|e| e.to_string())?;
            if n == 0 || !line.ends_with('\n') {
                return Err("Incomplete or oversized HTTP headers".into());
            }
            total += n;
            if total > 16 * 1024 {
                return Err("HTTP headers exceed16KiB".into());
            }
            if line == "\r\n" {
                break;
            }
            if status.is_none() {
                status = line.split_whitespace().nth(1).and_then(|s| s.parse().ok());
            } else if let Some((key, value)) = line.split_once(':') {
                if key.eq_ignore_ascii_case("content-length") {
                    if length.is_some() {
                        return Err("Duplicate response length".into());
                    }
                    length = Some(value.trim().parse::<u64>().map_err(|e| e.to_string())?);
                }
            }
            head.push_str(&line);
        }
        Ok(Self {
            status: status.ok_or_else(|| "Missing HTTP status".to_owned())?,
            remaining: length.ok_or_else(|| "Missing response length".to_owned())?,
            reader,
        })
    }
    fn bytes(&mut self, limit: usize) -> Result<Vec<u8>, String> {
        if self.remaining > limit as u64 {
            return Err("Response exceeds its size limit".into());
        }
        let mut bytes = vec![0; self.remaining as usize];
        self.reader
            .read_exact(&mut bytes)
            .map_err(|e| e.to_string())?;
        self.remaining = 0;
        Ok(bytes)
    }
}
fn error_message(status: u16, bytes: &[u8]) -> String {
    serde_json::from_slice::<Value>(bytes)
        .ok()
        .and_then(|v| v["error"].as_str().map(str::to_owned))
        .map(|m| format!("{m} (HTTP {status})"))
        .unwrap_or_else(|| format!("Request failed (HTTP {status})"))
}
fn encode(value: &str) -> String {
    value
        .as_bytes()
        .iter()
        .map(|b| {
            if b.is_ascii_alphanumeric() || b"-_.~".contains(b) {
                (*b as char).to_string()
            } else {
                format!("%{b:02X}")
            }
        })
        .collect()
}
fn path_query(value: &Value) -> String {
    if let Some(id) = value["path_id"].as_str() {
        format!("path_id={}", encode(id))
    } else {
        format!("path={}", encode(value["path"].as_str().unwrap_or("/")))
    }
}
fn child_path(parent: &str, name: &str) -> Result<String, String> {
    if name.is_empty() || name == "." || name == ".." || name.contains(['/', '\0']) {
        return Err("Folder entry has an invalid filename".into());
    }
    Ok(format!("{}/{}", parent.trim_end_matches('/'), name))
}

#[derive(Clone)]
enum Task {
    Upload {
        uri: String,
        destination: String,
        conflict: String,
        attach: bool,
    },
    SaveToFolder {
        entry: Value,
        uri: String,
    },
    FolderUpload {
        uri: String,
        destination: String,
        conflict: String,
    },
}
#[derive(Default)]
struct Resume {
    remote: Option<Value>,
    partial_id: Option<String>,
    source_id: Option<String>,
    destination_uri: Option<String>,
    completed_entries: std::collections::HashMap<String, String>,
    folder_documents: std::collections::HashMap<String, String>,
    staged_source: Option<(String, std::path::PathBuf, Value)>,
}
impl Drop for Resume {
    fn drop(&mut self) {
        if let Some((_, path, _)) = &self.staged_source {
            let _ = std::fs::remove_file(path);
        }
    }
}
struct Job {
    id: u64,
    label: String,
    task: Task,
    resume: Arc<Mutex<Resume>>,
    pause: Arc<AtomicBool>,
    cancel: Arc<AtomicBool>,
    state: String,
    done: u64,
    total: u64,
    running: bool,
}
enum Event {
    Listed(Result<Value, String>, bool),
    Roots(Result<Value, String>),
    Progress(u64, u64, u64),
    Finished(u64, Result<Option<String>, String>),
}

pub struct FileBrowser {
    connection: Option<Connection>,
    stop: Arc<AtomicBool>,
    tx: mpsc::Sender<Event>,
    rx: mpsc::Receiver<Event>,
    path: String,
    listing: Value,
    roots: Value,
    message: String,
    loading: bool,
    new_folder: String,
    conflict: String,
    pending_uris: Vec<String>,
    jobs: VecDeque<Job>,
    last_uploaded: Option<String>,
    insert_paths: VecDeque<String>,
    pending_downloads: Vec<Value>,
    selected: BTreeMap<String, Value>,
    sort: String,
    descending: bool,
    show_transfers: bool,
    cache_limit_bytes: u64,
    attach_next: bool,
    picker_generation: u64,
    picker_pending: bool,
    picker_destination: Option<String>,
    #[cfg(target_os = "android")]
    android: Option<crate::clipboard::AndroidClipboard>,
}
impl Default for FileBrowser {
    fn default() -> Self {
        let (tx, rx) = mpsc::channel();
        Self {
            connection: None,
            stop: Arc::new(AtomicBool::new(false)),
            tx,
            rx,
            path: String::new(),
            listing: Value::Null,
            roots: Value::Null,
            message: String::new(),
            loading: false,
            new_folder: String::new(),
            conflict: "skip".into(),
            pending_uris: vec![],
            jobs: VecDeque::new(),
            last_uploaded: None,
            insert_paths: VecDeque::new(),
            pending_downloads: Vec::new(),
            selected: BTreeMap::new(),
            sort: "name".into(),
            descending: false,
            show_transfers: false,
            cache_limit_bytes: 1024 * 1024 * 1024,
            attach_next: false,
            picker_generation: NEXT_JOB.fetch_add(1, Ordering::Relaxed),
            picker_pending: false,
            picker_destination: None,
            #[cfg(target_os = "android")]
            android: None,
        }
    }
}
impl Drop for FileBrowser {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        for job in &self.jobs {
            job.pause.store(true, Ordering::SeqCst);
        }
    }
}
impl FileBrowser {
    pub fn set_cache_limit_mib(&mut self, mib: u32) {
        self.cache_limit_bytes = u64::from(mib.clamp(64, 4096)) * 1024 * 1024;
    }
    #[cfg(target_os = "android")]
    pub fn set_android(&mut self, android: crate::clipboard::AndroidClipboard) {
        // Jobs are connection/process scoped; orphan staging cannot be resumed
        // after a killed process and must not consume the next session's budget.
        if let Ok(cache) = android.cache_directory() {
            let folder = cache.join("windowlink-transfers");
            if let Ok(entries) = std::fs::read_dir(folder) {
                for entry in entries.flatten() {
                    if entry.file_name().to_string_lossy().starts_with("source-")
                        && entry.file_type().is_ok_and(|kind| kind.is_file())
                    {
                        let _ = std::fs::remove_file(entry.path());
                    }
                }
            }
        }
        self.android = Some(android);
    }
    pub fn set_connection(&mut self, host: &str, port: u16, token: &str) {
        let connection = Connection {
            host: host.into(),
            port,
            token: token.into(),
        };
        if self.connection.as_ref() == Some(&connection) {
            return;
        }
        self.stop.store(true, Ordering::SeqCst);
        for job in &self.jobs {
            job.pause.store(true, Ordering::SeqCst);
        }
        self.stop = Arc::new(AtomicBool::new(false));
        let (tx, rx) = mpsc::channel();
        self.tx = tx;
        self.rx = rx;
        self.jobs.clear();
        self.pending_downloads.clear();
        self.selected.clear();
        self.attach_next = false;
        self.picker_generation = NEXT_JOB.fetch_add(1, Ordering::Relaxed);
        self.picker_pending = false;
        self.picker_destination = None;
        self.last_uploaded = None;
        self.insert_paths.clear();
        self.pending_uris.clear();
        self.path.clear();
        self.listing = Value::Null;
        self.roots = Value::Null;
        self.loading = false;
        self.connection = Some(connection.clone());
        let tx = self.tx.clone();
        thread::spawn(move || {
            let _ = tx.send(Event::Roots(connection.json("GET", "/fs/roots", None)));
        });
    }
    fn load(&mut self, path: Value, more: bool) {
        let Some(connection) = self.connection.clone() else {
            return;
        };
        if self.loading {
            return;
        }
        self.loading = true;
        if !more {
            self.selected.clear();
        }
        let mut query = format!(
            "{}&sort={}&descending={}",
            path_query(&path),
            self.sort,
            self.descending
        );
        if more {
            if let Some(cursor) = self.listing["next_cursor"].as_u64() {
                query.push_str(&format!("&cursor={cursor}"));
            }
            if let Some(id) = self.listing["directory_id"].as_str() {
                query.push_str(&format!("&directory_id={}", encode(id)));
            }
        }
        let tx = self.tx.clone();
        thread::spawn(move || {
            let _ = tx.send(Event::Listed(
                connection.json("GET", &format!("/fs/list?{query}"), None),
                more,
            ));
        });
    }
    pub fn poll(&mut self, ctx: &egui::Context) -> bool {
        while let Ok(event) = self.rx.try_recv() {
            match event {
                Event::Roots(Ok(value)) => {
                    self.path = value["home"].as_str().unwrap_or("/").into();
                    self.roots = value;
                    self.load(json!({"path":self.path}), false);
                }
                Event::Roots(Err(e)) => self.message = e,
                Event::Listed(result, more) => {
                    self.loading = false;
                    match result {
                        Ok(mut value) => {
                            if more {
                                if let (Some(old), Some(new)) = (
                                    self.listing["entries"].as_array_mut(),
                                    value["entries"].as_array_mut(),
                                ) {
                                    old.append(new);
                                }
                                self.listing["next_cursor"] = value["next_cursor"].clone();
                            } else {
                                self.path =
                                    value["path"].as_str().unwrap_or("Non-UTF-8 path").into();
                                self.listing = value;
                            }
                            self.message.clear();
                        }
                        Err(e) => {
                            // A failed replacement may have changed the requested sort.
                            // Never paginate the previous order using that new sort.
                            if !more && self.listing.is_object() {
                                self.listing["next_cursor"] = Value::Null;
                            }
                            self.message = e;
                        }
                    }
                }
                Event::Progress(id, done, total) => {
                    if let Some(job) = self.jobs.iter_mut().find(|j| j.id == id) {
                        job.done = done;
                        job.total = total;
                    }
                }
                Event::Finished(id, result) => {
                    if let Some(job) = self.jobs.iter_mut().find(|j| j.id == id) {
                        job.running = false;
                        match result {
                            Ok(path) => {
                                job.state = "Complete".into();
                                if let Some(path) = path {
                                    self.last_uploaded = Some(path.clone());
                                    if matches!(job.task, Task::Upload { attach: true, .. }) {
                                        self.insert_paths.push_back(path);
                                    }
                                }
                            }
                            Err(e) => job.state = e,
                        }
                    }
                }
            }
        }
        #[allow(unused_mut)]
        let mut shared = false;
        #[cfg(target_os = "android")]
        if let Some(android) = self.android.clone() {
            match android.take_shared_uris() {
                Ok(uris) if !uris.is_empty() => {
                    self.pending_uris.extend(uris);
                    self.pending_uris.truncate(128);
                    shared = true;
                }
                Err(e) => self.message = e,
                _ => {}
            }
            match android.take_transfer_result() {
                Ok(Some(value)) => {
                    if value["generation"].as_str() != Some(&self.picker_generation.to_string()) {
                        return shared;
                    }
                    self.picker_pending = false;
                    let picker_destination = self
                        .picker_destination
                        .take()
                        .unwrap_or_else(|| self.path.clone());
                    let kind = value["kind"].as_str().unwrap_or("");
                    let uris = value["uris"]
                        .as_array()
                        .map(|v| {
                            v.iter()
                                .filter_map(|u| u.as_str().map(str::to_owned))
                                .collect::<Vec<_>>()
                        })
                        .unwrap_or_default();
                    if uris.is_empty() {
                        self.pending_downloads.clear();
                        self.attach_next = false;
                    } else {
                        match kind {
                            "upload" => {
                                if self.attach_next {
                                    for uri in uris {
                                        self.enqueue(
                                            Task::Upload {
                                                uri,
                                                destination: picker_destination.clone(),
                                                conflict: self.conflict.clone(),
                                                attach: true,
                                            },
                                            "Attach file".into(),
                                        );
                                    }
                                    self.attach_next = false;
                                } else {
                                    self.pending_uris.extend(uris);
                                    self.pending_uris.truncate(128);
                                }
                            }
                            "folder_upload" => {
                                for uri in uris {
                                    self.enqueue(
                                        Task::FolderUpload {
                                            uri,
                                            destination: picker_destination.clone(),
                                            conflict: self.conflict.clone(),
                                        },
                                        "Upload folder contents".into(),
                                    );
                                }
                            }
                            "folder_save" => {
                                self.queue_selected_downloads(&uris[0]);
                            }
                            _ => {}
                        }
                    }
                }
                Ok(None) => {}
                Err(e) => self.message = e,
            }
        }
        self.start_next();
        if self.loading || self.jobs.iter().any(|j| j.running) {
            ctx.request_repaint_after(Duration::from_millis(100));
        }
        shared
    }
    fn queue_selected_downloads(&mut self, uri: &str) {
        for entry in std::mem::take(&mut self.pending_downloads) {
            let name = entry["name"].as_str().unwrap_or("Download").to_owned();
            self.enqueue(
                Task::SaveToFolder {
                    entry,
                    uri: uri.into(),
                },
                name,
            );
        }
        self.selected.clear();
    }
    fn begin_save(&mut self) {
        if self.selected.len() + self.jobs.len() > 128 {
            self.message = "Transfer queue is full. Clear finished transfers first".into();
            return;
        }
        self.pending_downloads = self.selected.values().cloned().collect();
        self.picker("folder_save", "");
    }
    fn enqueue(&mut self, task: Task, label: String) {
        if self.jobs.len() >= 128 {
            self.message = "Transfer queue is full".into();
            return;
        }
        self.jobs.push_back(Job {
            id: NEXT_JOB.fetch_add(1, Ordering::Relaxed),
            label,
            task,
            resume: Arc::new(Mutex::new(Resume::default())),
            pause: Arc::new(AtomicBool::new(false)),
            cancel: Arc::new(AtomicBool::new(false)),
            state: "Queued".into(),
            done: 0,
            total: 0,
            running: false,
        });
    }
    fn picker(&mut self, kind: &str, name: &str) {
        #[cfg(target_os = "android")]
        if let Some(android) = &self.android {
            match android.start_transfer_picker(kind, name, &self.picker_generation.to_string()) {
                Ok(()) => {
                    self.picker_pending = true;
                    self.picker_destination = Some(self.path.clone());
                }
                Err(e) => self.message = e,
            }
            return;
        }
        self.message = "File picking is available on Android".into();
        let _ = (kind, name);
    }
    pub fn picker_open(&self) -> bool {
        self.picker_pending
    }
    pub fn disconnect(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        for job in &self.jobs {
            job.pause.store(true, Ordering::SeqCst);
        }
        self.connection = None;
        self.picker_generation = NEXT_JOB.fetch_add(1, Ordering::SeqCst);
        self.picker_pending = false;
        self.picker_destination = None;
        self.pending_downloads.clear();
        self.selected.clear();
        self.attach_next = false;
        self.pending_uris.clear();
        self.insert_paths.clear();
        self.last_uploaded = None;
        self.jobs.clear();
        let (tx, rx) = mpsc::channel();
        self.tx = tx;
        self.rx = rx;
        self.loading = false;
    }
    pub fn request_attach(&mut self, cwd: &str) {
        if !cwd.is_empty() {
            self.path = cwd.into();
        }
        if self.path.is_empty() {
            self.message = "Choose a computer folder in Files first".into();
            return;
        }
        self.attach_next = true;
        self.picker("upload", "upload.bin");
    }
    pub fn take_insert_path(&mut self) -> Option<String> {
        self.insert_paths.pop_front()
    }
    pub fn show(&mut self, ui: &mut egui::Ui, back: &mut bool) -> Option<String> {
        ui.horizontal_wrapped(|ui| {
            if icons::button(ui, Icon::Back, "Back", true, false).clicked() {
                *back = true;
            }
            let count = self.selected.len();
            if icons::badge_button(
                ui,
                Icon::Save,
                "Save selected",
                count > 0 && !self.picker_pending && !self.loading,
                false,
                count,
            )
            .clicked()
            {
                self.begin_save();
            }
            let previous = (self.sort.clone(), self.descending);
            ui.add_enabled_ui(!self.loading, |ui| {
                icons::menu(ui, Icon::Sort, "Sort", |ui| {
                    for (key, label) in [
                        ("name", "Name"),
                        ("modified", "Date"),
                        ("size", "Size"),
                        ("type", "Type"),
                    ] {
                        ui.selectable_value(&mut self.sort, key.into(), label);
                    }
                    ui.separator();
                    ui.checkbox(&mut self.descending, "Descending");
                });
            });
            if previous != (self.sort.clone(), self.descending) {
                let selected = std::mem::take(&mut self.selected);
                self.load(self.listing.clone(), false);
                self.selected = selected;
            }
            icons::menu(ui, Icon::More, "More", |ui| self.actions(ui));
            if !self.jobs.is_empty()
                && icons::badge_button(
                    ui,
                    Icon::Transfers,
                    "Transfers",
                    true,
                    self.show_transfers,
                    self.jobs.len(),
                )
                .clicked()
            {
                self.show_transfers = !self.show_transfers;
            }
        });
        ui.horizontal(|ui| {
            if icons::button(ui, Icon::Up, "Parent folder", !self.loading, false).clicked() {
                if let Some(parent) = self.listing["parent_id"].as_str() {
                    self.load(json!({"path_id":parent}), false);
                }
            }
            let response = ui.add(
                egui::TextEdit::singleline(&mut self.path)
                    .desired_width((ui.available_width() - 80.0).max(40.0)),
            );
            let enter = response.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
            if icons::button(ui, Icon::Go, "Open path", !self.loading, false).clicked() || enter {
                self.load(json!({"path":self.path}), false);
            }
        });
        if !self.pending_uris.is_empty() {
            ui.horizontal_wrapped(|ui| {
                if ui
                    .button(format!("Upload {} here", self.pending_uris.len()))
                    .clicked()
                {
                    for uri in std::mem::take(&mut self.pending_uris) {
                        self.enqueue(
                            Task::Upload {
                                uri,
                                destination: self.path.clone(),
                                conflict: self.conflict.clone(),
                                attach: false,
                            },
                            "Upload file".into(),
                        );
                    }
                }
                if ui.button("Cancel").clicked() {
                    self.pending_uris.clear();
                }
            });
        }
        if !self.message.is_empty() {
            ui.label(&self.message);
        }
        if self.loading {
            ui.spinner();
        }
        if self.show_transfers && !self.jobs.is_empty() {
            egui::ScrollArea::vertical()
                .id_salt("transfer_jobs")
                .max_height((ui.available_height() * 0.4).min(180.0))
                .show(ui, |ui| self.show_jobs(ui));
        }
        let entries = self.listing["entries"]
            .as_array()
            .cloned()
            .unwrap_or_default();
        ui.horizontal(|ui| {
            let selectable: Vec<_> = entries
                .iter()
                .filter(|e| matches!(e["kind"].as_str(), Some("file" | "directory")))
                .collect();
            let mut all = !selectable.is_empty()
                && selectable
                    .iter()
                    .all(|e| self.selected.contains_key(&path_query(e)));
            if ui
                .add_enabled(!self.loading, egui::Checkbox::without_text(&mut all))
                .on_hover_text(if self.listing["next_cursor"].is_null() {
                    "Select all"
                } else {
                    "Select loaded"
                })
                .changed()
            {
                if all {
                    for entry in selectable {
                        self.selected.insert(path_query(entry), entry.clone());
                    }
                } else {
                    self.selected.clear();
                }
            }
            ui.weak("Name");
        });
        egui::ScrollArea::vertical()
            .id_salt("file_list")
            .auto_shrink([false, false])
            .show(ui, |ui| {
                ui.add_enabled_ui(!self.loading, |ui| {
                    for entry in entries {
                        let key = path_query(&entry);
                        let name = entry["name"].as_str().unwrap_or("File");
                        let kind = entry["kind"].as_str().unwrap_or("file");
                        let can_save = matches!(kind, "file" | "directory");
                        let directory = kind == "directory";
                        ui.push_id(&key, |ui| {
                            ui.horizontal(|ui| {
                                ui.set_min_height(42.0);
                                let mut checked = self.selected.contains_key(&key);
                                if ui
                                    .add_enabled(
                                        can_save,
                                        egui::Checkbox::without_text(&mut checked),
                                    )
                                    .changed()
                                {
                                    if checked {
                                        self.selected.insert(key.clone(), entry.clone());
                                    } else {
                                        self.selected.remove(&key);
                                    }
                                }
                                icons::small(ui, if directory { Icon::Folder } else { Icon::File });
                                let label = name.to_owned();
                                let response = ui.add_sized(
                                    [ui.available_width(), 42.0],
                                    egui::Button::new(egui::RichText::new(label).size(18.0))
                                        .right_text("")
                                        .frame(false)
                                        .wrap()
                                        .selected(checked),
                                );
                                if response.clicked() {
                                    if directory || kind == "symlink" {
                                        self.load(entry.clone(), false);
                                    } else if can_save {
                                        if checked {
                                            self.selected.remove(&key);
                                        } else {
                                            self.selected.insert(key.clone(), entry.clone());
                                        }
                                    }
                                }
                            });
                        });
                        ui.separator();
                    }
                    if self.listing["next_cursor"].as_u64().is_some()
                        && ui.button("Load more").clicked()
                    {
                        self.load(self.listing.clone(), true);
                    }
                    if self.listing["entries"]
                        .as_array()
                        .is_some_and(Vec::is_empty)
                    {
                        ui.weak("Empty folder");
                    }
                });
            });
        self.take_insert_path()
    }
    fn actions(&mut self, ui: &mut egui::Ui) {
        if ui.button("Upload files").clicked() {
            self.picker("upload", "");
            ui.close();
        }
        if ui.button("Upload folder").clicked() {
            self.picker("folder_upload", "");
            ui.close();
        }
        ui.separator();
        for (label, path) in [
            (
                "Home",
                self.roots["home"].as_str().unwrap_or("/").to_owned(),
            ),
            ("Root", "/".into()),
        ] {
            if ui.button(label).clicked() {
                self.load(json!({"path":path}), false);
                ui.close();
            }
        }
        if ui.button("Refresh").clicked() {
            self.load(self.listing.clone(), false);
            ui.close();
        }
        ui.menu_button("Drives", |ui| {
            for mount in self.roots["mounts"].as_array().cloned().unwrap_or_default() {
                if let Some(path) = mount.as_str() {
                    if ui.button(path).clicked() {
                        self.load(json!({"path":path}), false);
                        ui.close();
                    }
                }
            }
        });
        ui.collapsing("New folder", |ui| {
            ui.add(
                egui::TextEdit::singleline(&mut self.new_folder)
                    .hint_text("Name")
                    .desired_width(180.0),
            );
            if ui
                .add_enabled(
                    !self.loading && !self.new_folder.is_empty(),
                    egui::Button::new("Create"),
                )
                .clicked()
            {
                if let Ok(path) = child_path(&self.path, &self.new_folder) {
                    if let Some(connection) = self.connection.clone() {
                        let tx = self.tx.clone();
                        let query = format!(
                            "{}&sort={}&descending={}",
                            path_query(&self.listing),
                            self.sort,
                            self.descending
                        );
                        self.loading = true;
                        self.selected.clear();
                        thread::spawn(move || {
                            let result = connection
                                .json("POST", "/fs/mkdir", Some(json!({"path":path})))
                                .and_then(|_| {
                                    connection.json("GET", &format!("/fs/list?{query}"), None)
                                });
                            let _ = tx.send(Event::Listed(result, false));
                        });
                    }
                    self.new_folder.clear();
                    ui.close();
                }
            }
        });
        ui.collapsing("Upload conflicts", |ui| {
            for (policy, label) in [
                ("skip", "Skip"),
                ("keep_both", "Keep both"),
                ("replace", "Replace"),
            ] {
                ui.selectable_value(&mut self.conflict, policy.into(), label);
            }
        });
    }
    fn show_jobs(&mut self, ui: &mut egui::Ui) {
        for job in &mut self.jobs {
            ui.separator();
            ui.label(&job.label);
            ui.label(&job.state);
            if job.total > 0 {
                ui.add(
                    egui::ProgressBar::new(job.done as f32 / job.total as f32)
                        .text(format!("{} / {} bytes", job.done, job.total)),
                );
            }
            if !job.running && job.state.contains("atomic upload requires") {
                ui.label("In-place replacement writes the existing file directly; failure can leave partial contents.");
                if ui.button("Replace in place").clicked() {
                    if let Task::Upload { conflict, .. } = &mut job.task {
                        *conflict = "replace_in_place".into();
                        job.pause.store(false, Ordering::SeqCst);
                        job.cancel.store(false, Ordering::SeqCst);
                        job.state = "Queued".into();
                    }
                }
            }
            ui.horizontal_wrapped(|ui| {
                if job.running && ui.button("Pause").clicked() {
                    job.pause.store(true, Ordering::SeqCst);
                }
                if !job.running && job.state != "Complete" && ui.button("Resume").clicked() {
                    job.pause.store(false, Ordering::SeqCst);
                    job.cancel.store(false, Ordering::SeqCst);
                    job.state = "Queued".into();
                }
                if job.state != "Complete" && ui.button("Cancel").clicked() {
                    job.cancel.store(true, Ordering::SeqCst);
                    job.pause.store(true, Ordering::SeqCst);
                    job.state = "Cancel requested".into();
                    if !job.running {
                        let connection = self.connection.clone();
                        let resume = job.resume.clone();
                        let tx = self.tx.clone();
                        let id = job.id;
                        thread::spawn(move || {
                            let mut resume = resume.lock().unwrap_or_else(|e| e.into_inner());
                            if let (Some(connection), Some(remote)) =
                                (connection, resume.remote.take())
                            {
                                if let Some(id) = remote["id"].as_str() {
                                    let _ = connection.json(
                                        "DELETE",
                                        &format!("/transfers/{id}?retain=false"),
                                        None,
                                    );
                                }
                            }
                            if let Some((_, path, _)) = resume.staged_source.take() {
                                let _ = std::fs::remove_file(path);
                            }
                            let _ = tx.send(Event::Finished(id, Err("Cancelled".into())));
                        });
                    }
                }
            });
        }
        if ui.button("Clear completed transfers").clicked() {
            self.jobs
                .retain(|j| j.state != "Complete" && j.state != "Cancelled");
        }
        if let Some(path) = self.last_uploaded.clone() {
            ui.label(&path);
            if ui.button("Insert path in terminal").clicked() {
                self.insert_paths.push_back(path);
            }
        }
    }

    fn start_next(&mut self) {
        if self.jobs.iter().any(|j| j.running) {
            return;
        }
        #[cfg(target_os = "android")]
        if let (Some(connection), Some(android)) = (self.connection.clone(), self.android.clone()) {
            if let Some(job) = self.jobs.iter_mut().find(|j| j.state == "Queued") {
                job.running = true;
                job.state = "Transferring".into();
                let task = job.task.clone();
                let id = job.id;
                let resume = job.resume.clone();
                let pause = job.pause.clone();
                let cancel = job.cancel.clone();
                let stop = self.stop.clone();
                let tx = self.tx.clone();
                let cache_limit_bytes = self.cache_limit_bytes;
                let chunk_limit = self.roots["limits"]["transfer_chunk_max_bytes"]
                    .as_u64()
                    .unwrap_or(4 * 1024 * 1024)
                    .clamp(64 * 1024, 64 * 1024 * 1024);
                thread::spawn(move || {
                    let context = TransferContext {
                        cache_limit_bytes,
                        chunk_limit,
                        connection,
                        android,
                        id,
                        resume,
                        pause,
                        cancel,
                        stop,
                        tx: tx.clone(),
                    };
                    let result = context.run(task);
                    let _ = tx.send(Event::Finished(id, result));
                });
            }
        }
    }
}

#[cfg(target_os = "android")]
struct TransferContext {
    cache_limit_bytes: u64,
    chunk_limit: u64,
    connection: Connection,
    android: crate::clipboard::AndroidClipboard,
    id: u64,
    resume: Arc<Mutex<Resume>>,
    pause: Arc<AtomicBool>,
    cancel: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
    tx: mpsc::Sender<Event>,
}
#[cfg(target_os = "android")]
impl TransferContext {
    fn check(&self) -> Result<(), String> {
        if self.stop.load(Ordering::SeqCst)
            || self.pause.load(Ordering::SeqCst)
            || self.cancel.load(Ordering::SeqCst)
        {
            if let Some(remote) = self
                .resume
                .lock()
                .map_err(|_| "Transfer state unavailable".to_owned())?
                .remote
                .as_ref()
            {
                if let Some(id) = remote["id"].as_str() {
                    let _ = self.connection.json(
                        "DELETE",
                        &format!(
                            "/transfers/{id}?retain={}",
                            !self.cancel.load(Ordering::SeqCst)
                        ),
                        None,
                    );
                }
            }
            return Err(if self.cancel.load(Ordering::SeqCst) {
                "Cancelled"
            } else {
                "Paused; tap Resume"
            }
            .into());
        }
        Ok(())
    }
    fn run(&self, task: Task) -> Result<Option<String>, String> {
        match task {
            Task::Upload {
                uri,
                destination,
                conflict,
                ..
            } => self.upload(&uri, &destination, &conflict).map(Some),

            Task::FolderUpload {
                uri,
                destination,
                conflict,
            } => {
                self.upload_folder(
                    &uri,
                    &destination,
                    &conflict,
                    0,
                    &mut std::collections::HashSet::new(),
                )?;
                Ok(None)
            }
            Task::SaveToFolder { entry, uri } => {
                self.check()?;
                let key = path_query(&entry);
                let cached = self
                    .resume
                    .lock()
                    .map_err(|_| "Transfer state unavailable")?
                    .folder_documents
                    .get(&key)
                    .cloned();
                let directory = entry["kind"].as_str() == Some("directory");
                let destination = if let Some(cached) = cached {
                    cached
                } else {
                    let created = self.android.create_document(
                        &uri,
                        entry["name"].as_str().unwrap_or("file"),
                        directory,
                    )?;
                    self.resume
                        .lock()
                        .map_err(|_| "Transfer state unavailable")?
                        .folder_documents
                        .insert(key, created.clone());
                    created
                };
                if directory {
                    self.download_folder(
                        &entry,
                        &destination,
                        0,
                        &mut std::collections::HashSet::new(),
                    )?;
                } else {
                    self.download(&entry, &destination)?;
                }
                Ok(None)
            }
        }
    }
    fn source_identity(
        &self,
        file: &mut std::fs::File,
        uri: &str,
    ) -> Result<(u64, String), String> {
        use sha2::{Digest, Sha256};
        use std::io::{Seek, SeekFrom};
        file.seek(SeekFrom::Start(0)).map_err(|_| {
            "Selected document cannot be resumed; choose a seekable document provider".to_owned()
        })?;
        let mut hash = Sha256::new();
        let mut buffer = [0; BUFFER];
        let mut size = 0;
        loop {
            self.check()?;
            let n = file.read(&mut buffer).map_err(|e| e.to_string())?;
            if n == 0 {
                break;
            }
            size += n as u64;
            hash.update(&buffer[..n]);
        }
        file.seek(SeekFrom::Start(0)).map_err(|e| e.to_string())?;
        let metadata = {
            let resume = self
                .resume
                .lock()
                .map_err(|_| "Transfer state unavailable".to_owned())?;
            match &resume.staged_source {
                Some((source, _, metadata)) if source == uri => metadata.clone(),
                _ => self.android.document_metadata(uri)?,
            }
        };
        hash.update(metadata.to_string().as_bytes());
        Ok((size, format!("{:x}", hash.finalize())))
    }
    fn upload_source(&self, uri: &str) -> Result<(std::fs::File, Value), String> {
        use std::io::{Seek, SeekFrom};
        if let Some((source, path, metadata)) = &self
            .resume
            .lock()
            .map_err(|_| "Transfer state unavailable".to_owned())?
            .staged_source
        {
            if source == uri {
                return Ok((
                    std::fs::File::open(path).map_err(|e| e.to_string())?,
                    metadata.clone(),
                ));
            }
        }
        let metadata = self.android.document_metadata(uri)?;
        let mut input = self.android.open_document(uri, "r")?;
        let seekable = input.seek(SeekFrom::Current(0)).is_ok();
        if seekable && metadata["persisted"].as_bool() == Some(true) {
            return Ok((input, metadata));
        }
        // Temporary URI grants and pipes become a stable private source snapshot.
        // The staging cache is bounded independently of ordinary seekable files.
        let directory = self.android.cache_directory()?.join("windowlink-transfers");
        std::fs::create_dir_all(&directory).map_err(|e| e.to_string())?;
        let used = std::fs::read_dir(&directory)
            .map_err(|e| e.to_string())?
            .filter_map(Result::ok)
            .filter_map(|e| e.metadata().ok())
            .map(|m| m.len())
            .sum::<u64>();
        let budget = self.cache_limit_bytes.saturating_sub(used);
        let path = directory.join(format!(
            "source-{}-{}",
            self.id,
            NEXT_JOB.fetch_add(1, Ordering::Relaxed)
        ));
        let mut output = std::fs::OpenOptions::new()
            .create_new(true)
            .read(true)
            .write(true)
            .open(&path)
            .map_err(|e| e.to_string())?;
        let result = (|| {
            let mut buffer = [0; BUFFER];
            let mut length = 0;
            loop {
                self.check()?;
                let n = input.read(&mut buffer).map_err(|e| e.to_string())?;
                if n == 0 {
                    break;
                }
                length += n as u64;
                if length > budget {
                    return Err("Temporary file staging exceeds the configured cache limit; choose a persistable document".to_owned());
                }
                output.write_all(&buffer[..n]).map_err(|e| e.to_string())?;
            }
            output.sync_all().map_err(|e| e.to_string())?;
            output.seek(SeekFrom::Start(0)).map_err(|e| e.to_string())?;
            Ok(())
        })();
        if let Err(e) = result {
            let _ = std::fs::remove_file(path);
            return Err(e);
        }
        let mut resume = self
            .resume
            .lock()
            .map_err(|_| "Transfer state unavailable".to_owned())?;
        if let Some((_, old, _)) = resume.staged_source.take() {
            let _ = std::fs::remove_file(old);
        }
        resume.staged_source = Some((uri.into(), path, metadata.clone()));
        Ok((output, metadata))
    }
    fn upload(&self, uri: &str, destination: &str, conflict: &str) -> Result<String, String> {
        use std::io::{Seek, SeekFrom};
        let (mut file, metadata) = self.upload_source(uri)?;
        let name = metadata["name"].as_str().unwrap_or("upload.bin").to_owned();
        let path = child_path(destination, &name)?;
        let (size, source_id) = self.source_identity(&mut file, uri)?;
        let mut remote = {
            let resume = self
                .resume
                .lock()
                .map_err(|_| "Transfer state unavailable".to_owned())?;
            match &resume.remote{Some(value) if value["path"].as_str()==Some(&path)&&resume.source_id.as_deref()==Some(&source_id)=>self.connection.json("GET",&format!("/transfers/{}",value["id"].as_str().unwrap_or("")),None)?,Some(value) if value["path"].as_str()==Some(&path)=>return Err("Selected source changed; cancel and restart".into()),_=>self.connection.json("POST","/transfers",Some(json!({"direction":"upload","path":path,"size":size,"source_id":source_id,"conflict":conflict})))?}
        };
        {
            let mut resume = self
                .resume
                .lock()
                .map_err(|_| "Transfer state unavailable".to_owned())?;
            resume.remote = Some(remote.clone());
            resume.source_id = Some(source_id.clone());
        }
        let id = remote["id"]
            .as_str()
            .ok_or_else(|| "Transfer ID missing".to_owned())?
            .to_owned();
        let mut offset = remote["offset"].as_u64().unwrap_or(0);
        if offset > size {
            return Err("Host partial exceeds source size".into());
        }
        file.seek(SeekFrom::Start(offset))
            .map_err(|e| e.to_string())?;
        let mut buffer = [0; BUFFER];
        while offset < size {
            self.check()?;
            let chunk = (size - offset).min(self.chunk_limit);
            let mut stream = self.connection.connect(
                "PATCH",
                &format!(
                    "/transfers/{id}?offset={offset}&source_id={}",
                    encode(&source_id)
                ),
                "application/octet-stream",
                chunk,
            )?;
            let mut remaining = chunk;
            while remaining > 0 {
                self.check()?;
                let n = file
                    .read(&mut buffer[..remaining.min(BUFFER as u64) as usize])
                    .map_err(|e| e.to_string())?;
                if n == 0 {
                    return Err("Selected source truncated during upload".into());
                }
                stream.write_all(&buffer[..n]).map_err(|e| e.to_string())?;
                offset += n as u64;
                remaining -= n as u64;
                let _ = self.tx.send(Event::Progress(self.id, offset, size));
            }
            let mut response = Response::new(stream)?;
            let bytes = response.bytes(64 * 1024)?;
            if response.status != 200 {
                return Err(error_message(response.status, &bytes));
            }
            remote = serde_json::from_slice(&bytes).map_err(|e| e.to_string())?;
            self.resume
                .lock()
                .map_err(|_| "Transfer state unavailable".to_owned())?
                .remote = Some(remote.clone());
        }
        self.check()?;
        let (final_size, final_id) = self.source_identity(&mut file, uri)?;
        if final_size != size || final_id != source_id {
            return Err("Selected source changed during upload; cancel and restart".into());
        }
        let completed = self.connection.json(
            "POST",
            &format!("/transfers/{id}/complete?source_id={}", encode(&source_id)),
            None,
        )?;
        {
            let mut resume = self
                .resume
                .lock()
                .map_err(|_| "Transfer state unavailable".to_owned())?;
            resume.remote = None;
            if let Some((_, path, _)) = resume.staged_source.take() {
                let _ = std::fs::remove_file(path);
            }
        }
        completed["path"]
            .as_str()
            .map(str::to_owned)
            .ok_or_else(|| "Uploaded path is not UTF-8; browse it in Files".into())
    }
    fn download(&self, entry: &Value, uri: &str) -> Result<(), String> {
        use std::io::{Seek, SeekFrom};
        use std::os::unix::fs::MetadataExt;
        let mut file = self.android.open_document(uri, "rw")?;
        let destination_identity = |file: &std::fs::File| -> Result<String, String> {
            let m = file.metadata().map_err(|e| e.to_string())?;
            Ok(format!(
                "{}:{}:{}:{}:{}:{}:{}",
                m.dev(),
                m.ino(),
                m.len(),
                m.mtime(),
                m.mtime_nsec(),
                m.ctime(),
                m.ctime_nsec()
            ))
        };
        let remote = {
            let resume = self
                .resume
                .lock()
                .map_err(|_| "Transfer state unavailable".to_owned())?;
            if let Some(remote) = &resume.remote {
                if remote["path_id"] != entry["path_id"]
                    || resume.destination_uri.as_deref() != Some(uri)
                {
                    return Err("A different file has a retained partial; resume it or cancel the folder transfer".into());
                }
                if let Some(partial) = &resume.partial_id {
                    if partial != &destination_identity(&file)? {
                        return Err("Download destination changed; cancel and restart".into());
                    }
                }
                self.connection.json(
                    "GET",
                    &format!("/transfers/{}", remote["id"].as_str().unwrap_or("")),
                    None,
                )?
            } else {
                self.connection.json("POST","/transfers",Some(json!({"direction":"download","path":entry["path"],"path_id":entry["path_id"]})))?
            }
        };
        let id = remote["id"]
            .as_str()
            .ok_or_else(|| "Transfer ID missing".to_owned())?;
        let source = remote["source_id"]
            .as_str()
            .ok_or_else(|| "Source identity missing".to_owned())?;
        let size = remote["size"]
            .as_u64()
            .ok_or_else(|| "Source size missing".to_owned())?;
        let offset = file.seek(SeekFrom::End(0)).map_err(|_| {
            "Download destination cannot resume; choose Android Downloads".to_owned()
        })?;
        if offset > size {
            return Err("Download destination exceeds source size".into());
        }
        if offset > 0
            && self
                .resume
                .lock()
                .map_err(|_| "Transfer state unavailable".to_owned())?
                .partial_id
                .is_none()
        {
            return Err("Download destination is not empty; choose a new file".into());
        }
        {
            let mut resume = self
                .resume
                .lock()
                .map_err(|_| "Transfer state unavailable".to_owned())?;
            resume.remote = Some(remote.clone());
            resume.destination_uri = Some(uri.to_owned());
            resume.partial_id = Some(destination_identity(&file)?);
        }
        let stream = self.connection.connect(
            "GET",
            &format!(
                "/transfers/{id}/download?offset={offset}&source_id={}",
                encode(source)
            ),
            "application/octet-stream",
            0,
        )?;
        let mut response = Response::new(stream)?;
        if response.status != 200 && response.status != 206 {
            let bytes = response.bytes(64 * 1024)?;
            return Err(error_message(response.status, &bytes));
        }
        if response.remaining != size - offset {
            return Err("Download length changed".into());
        }
        let mut done = offset;
        let mut buffer = [0; BUFFER];
        let result = (|| {
            while response.remaining > 0 {
                self.check()?;
                let n = response
                    .reader
                    .read(&mut buffer[..response.remaining.min(BUFFER as u64) as usize])
                    .map_err(|e| e.to_string())?;
                if n == 0 {
                    return Err("Download interrupted; tap Resume".to_owned());
                }
                file.write_all(&buffer[..n]).map_err(|e| e.to_string())?;
                response.remaining -= n as u64;
                done += n as u64;
                let _ = self.tx.send(Event::Progress(self.id, done, size));
            }
            file.sync_all().map_err(|e| e.to_string())?;
            self.connection
                .json("GET", &format!("/transfers/{id}"), None)?;
            Ok(())
        })();
        self.resume
            .lock()
            .map_err(|_| "Transfer state unavailable".to_owned())?
            .partial_id = Some(destination_identity(&file)?);
        result?;
        let _ = self
            .connection
            .json("DELETE", &format!("/transfers/{id}?retain=false"), None);
        {
            let mut resume = self
                .resume
                .lock()
                .map_err(|_| "Transfer state unavailable".to_owned())?;
            resume.remote = None;
            resume.partial_id = None;
            resume.destination_uri = None;
        }
        Ok(())
    }
    fn upload_folder(
        &self,
        uri: &str,
        destination: &str,
        conflict: &str,
        depth: usize,
        visited: &mut std::collections::HashSet<String>,
    ) -> Result<(), String> {
        if depth >= 64 || visited.len() >= 10000 || !visited.insert(uri.into()) {
            return Err("Folder tree is too deep or contains a loop".into());
        }
        let mut cursor = 0;
        loop {
            self.check()?;
            let listing = self.android.tree_entries(uri, cursor)?;
            for entry in listing["entries"].as_array().cloned().unwrap_or_default() {
                let child = entry["uri"]
                    .as_str()
                    .ok_or_else(|| "Folder entry missing URI".to_owned())?;
                let name = entry["name"]
                    .as_str()
                    .ok_or_else(|| "Folder entry missing name".to_owned())?;
                if entry["directory"].as_bool() == Some(true) {
                    let path = child_path(destination, name)?;
                    match self
                        .connection
                        .json("POST", "/fs/mkdir", Some(json!({"path":path})))
                    {
                        Ok(_) => {}
                        Err(e) if e.contains("HTTP 409") => {}
                        Err(e) => return Err(e),
                    }
                    self.upload_folder(child, &path, conflict, depth + 1, visited)?;
                } else {
                    if self
                        .resume
                        .lock()
                        .map_err(|_| "Transfer state unavailable".to_owned())?
                        .completed_entries
                        .contains_key(child)
                    {
                        continue;
                    }
                    match self.upload(child, destination, conflict) {
                        Ok(_) => {
                            let mut resume = self
                                .resume
                                .lock()
                                .map_err(|_| "Transfer state unavailable".to_owned())?;
                            if resume.completed_entries.len() >= 10000 {
                                return Err("Folder resume index exceeds 10000 files".into());
                            }
                            resume
                                .completed_entries
                                .insert(child.into(), "complete".into());
                        }
                        Err(e)
                            if conflict == "skip"
                                && e.contains("HTTP 409")
                                && e.contains("destination exists") => {}
                        Err(e) => return Err(e),
                    }
                }
            }
            if let Some(next) = listing["next_cursor"].as_i64() {
                cursor = next as i32;
            } else {
                break;
            }
        }
        Ok(())
    }
    fn download_folder(
        &self,
        entry: &Value,
        uri: &str,
        depth: usize,
        visited: &mut std::collections::HashSet<String>,
    ) -> Result<(), String> {
        let id = entry["path_id"].as_str().unwrap_or("");
        if depth >= 64 || visited.len() >= 10000 || !visited.insert(id.into()) {
            return Err("Folder tree is too deep or contains a loop".into());
        }
        let mut cursor = 0;
        let mut directory_id = None::<String>;
        loop {
            self.check()?;
            let listing = self.connection.json(
                "GET",
                &format!(
                    "/fs/list?{}&cursor={cursor}{}",
                    path_query(entry),
                    directory_id
                        .as_ref()
                        .map(|id| format!("&directory_id={}", encode(id)))
                        .unwrap_or_default()
                ),
                None,
            )?;
            directory_id = listing["directory_id"].as_str().map(str::to_owned);
            for child in listing["entries"].as_array().cloned().unwrap_or_default() {
                let name = child["name"].as_str().unwrap_or("file");
                match child["kind"].as_str() {
                    Some("directory") => {
                        let key = child["path_id"].as_str().unwrap_or("").to_owned();
                        let cached = self
                            .resume
                            .lock()
                            .map_err(|_| "Transfer state unavailable".to_owned())?
                            .folder_documents
                            .get(&key)
                            .cloned();
                        let created = if let Some(uri) = cached {
                            uri
                        } else {
                            let created = self.android.create_document(uri, name, true)?;
                            self.resume
                                .lock()
                                .map_err(|_| "Transfer state unavailable".to_owned())?
                                .folder_documents
                                .insert(key, created.clone());
                            created
                        };
                        self.download_folder(&child, &created, depth + 1, visited)?;
                    }
                    Some("file") => {
                        let key = child["path_id"].as_str().unwrap_or("").to_owned();
                        let source = child["source_id"].as_str().unwrap_or("").to_owned();
                        {
                            let resume = self
                                .resume
                                .lock()
                                .map_err(|_| "Transfer state unavailable".to_owned())?;
                            if let Some(done) = resume.completed_entries.get(&key) {
                                if done == &source {
                                    continue;
                                }
                                return Err(
                                    "Folder source changed; restart into a new destination".into(),
                                );
                            }
                        }
                        let cached = self
                            .resume
                            .lock()
                            .map_err(|_| "Transfer state unavailable".to_owned())?
                            .folder_documents
                            .get(&key)
                            .cloned();
                        let created = if let Some(uri) = cached {
                            uri
                        } else {
                            let created = self.android.create_document(uri, name, false)?;
                            self.resume
                                .lock()
                                .map_err(|_| "Transfer state unavailable".to_owned())?
                                .folder_documents
                                .insert(key.clone(), created.clone());
                            created
                        };
                        self.download(&child, &created)?;
                        let mut resume = self
                            .resume
                            .lock()
                            .map_err(|_| "Transfer state unavailable".to_owned())?;
                        if resume.completed_entries.len() >= 10000 {
                            return Err("Folder resume index exceeds 10000 files".into());
                        }
                        resume.completed_entries.insert(key, source);
                    }
                    Some("symlink") | Some("special") => {}
                    _ => {}
                }
            }
            if let Some(next) = listing["next_cursor"].as_u64() {
                cursor = next;
            } else {
                break;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn batch_save_uses_snapshot_and_one_destination_for_files_and_folders() {
        let mut browser = FileBrowser::default();
        let a = json!({"path_id":"aa","name":"a.txt","kind":"file"});
        let b = json!({"path_id":"bb","name":"folder","kind":"directory"});
        browser.selected.insert(path_query(&a), a.clone());
        browser.selected.insert(path_query(&b), b.clone());
        browser.begin_save();
        // Later UI changes must not replace the entries chosen for this picker.
        browser.selected.clear();
        browser.queue_selected_downloads("content://destination");
        assert_eq!(browser.jobs.len(), 2);
        for (job, expected) in browser.jobs.iter().zip([a, b]) {
            match &job.task {
                Task::SaveToFolder { entry, uri } => {
                    assert_eq!(entry, &expected);
                    assert_eq!(uri, "content://destination");
                }
                _ => panic!("Expected a download"),
            }
        }
        assert!(browser.pending_downloads.is_empty());
    }
    #[test]
    fn disconnect_discards_selection_and_pending_batch() {
        let mut browser = FileBrowser::default();
        let entry = json!({"path_id":"ff","name":"file","kind":"file"});
        browser.selected.insert(path_query(&entry), entry);
        browser.begin_save();
        browser.disconnect();
        browser.queue_selected_downloads("content://late-picker");
        assert!(browser.jobs.is_empty());
        assert!(browser.selected.is_empty());
    }
    #[test]
    fn failed_sort_reload_cannot_mix_pagination_orders() {
        let mut browser = FileBrowser::default();
        browser.listing = json!({"entries":[],"next_cursor":200});
        browser.loading = true;
        browser
            .tx
            .send(Event::Listed(Err("Offline".into()), false))
            .unwrap();
        browser.poll(&egui::Context::default());
        assert!(browser.listing["next_cursor"].is_null());
        assert!(!browser.loading);
    }
    #[test]
    fn file_browser_fits_portrait_and_landscape_with_selection() {
        for size in [egui::vec2(360.0, 800.0), egui::vec2(800.0, 360.0)] {
            let ctx = egui::Context::default();
            crate::theme::apply(&ctx, crate::theme::Theme::TokyoNight.palette());
            let mut browser = FileBrowser::default();
            browser.path = "/home/user/Some long folder name".into();
            let entries: Vec<_> = (0..12).map(|i| json!({"name":format!("File {i}.txt"),"path_id":format!("{i:02x}"),"kind":"file"})).collect();
            browser.selected = entries.iter().map(|e| (path_query(e), e.clone())).collect();
            browser.listing = json!({"entries":entries});
            browser.enqueue(
                Task::SaveToFolder {
                    entry: Value::Null,
                    uri: "content://test".into(),
                },
                "Test transfer".into(),
            );
            for _ in 0..3 {
                let mut output = ctx.run_ui(
                    egui::RawInput {
                        screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, size)),
                        ..Default::default()
                    },
                    |ctx| {
                        egui::CentralPanel::default().show(ctx, |ui| {
                            browser.show(ui, &mut false);
                            assert!(
                                ui.min_rect().right() <= size.x,
                                "Browser overflows {size:?}: {:?}",
                                ui.min_rect()
                            );
                            assert!(
                                ui.min_rect().bottom() <= size.y,
                                "Browser controls overflow {size:?}: {:?}",
                                ui.min_rect()
                            );
                        });
                    },
                );
                output.textures_delta.clear();
            }
        }
    }
    #[test]
    fn encoded_paths_preserve_names() {
        assert_eq!(encode("/tmp/a '日本"), "%2Ftmp%2Fa%20%27%E6%97%A5%E6%9C%AC");
        assert!(child_path("/tmp", "..").is_err());
        assert_eq!(child_path("/", "hidden"), Ok("/hidden".into()));
    }
    #[test]
    fn connection_change_discards_old_events_and_jobs() {
        let mut browser = FileBrowser::default();
        browser.connection = Some(Connection {
            host: "old".into(),
            port: 1,
            token: "old".into(),
        });
        let old = browser.tx.clone();
        browser.set_connection("127.0.0.1", 1, "new");
        assert!(old.send(Event::Progress(1, 1, 1)).is_err());
        assert!(browser.jobs.is_empty());
    }
}
