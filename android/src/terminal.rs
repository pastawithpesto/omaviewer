//! Native terminal view backed by Alacritty's VT parser and persistent host tmux.
//! The network worker owns one foreground PTY; switching views detaches it.
use crate::storage::ControlAlignment;
use alacritty_terminal::{
    event::{Event as TerminalEvent, EventListener, WindowSize},
    grid::{Dimensions, Scroll},
    index::{Column, Line, Point, Side},
    selection::{Selection, SelectionType},
    term::{Config, Term, TermMode, cell::Flags, color::Colors},
    vte::ansi::{Color, CursorShape, NamedColor, Processor, Rgb},
};
use eframe::egui::{self, Color32, FontId, Pos2, Rect, Vec2};
use serde_json::{Value, json};
use std::{
    sync::mpsc::{self, Receiver, SyncSender, TrySendError},
    thread,
    time::{Duration, Instant},
};

pub fn install_font(ctx: &egui::Context) {
    let mut fonts = egui::FontDefinitions::default();
    fonts.font_data.insert(
        "terminal-mono".into(),
        egui::FontData::from_static(include_bytes!(
            "../assets/fonts/JetBrainsMonoNerdFont-Regular.ttf"
        ))
        .into(),
    );
    fonts
        .families
        .entry(egui::FontFamily::Monospace)
        .or_default()
        .insert(0, "terminal-mono".into());
    fonts
        .families
        .entry(egui::FontFamily::Proportional)
        .or_default()
        .push("terminal-mono".into());
    ctx.set_fonts(fonts);
}

#[derive(Debug)]
pub enum TerminalAction {
    Back,
    ShowKeyboard,
    Copy(String),
    Paste,
    AttachFile { working_directory: String },
}
#[derive(Clone, Debug)]
struct Session {
    id: String,
    name: String,
    working_directory: String,
}
impl Session {
    fn parse(value: &Value) -> Option<Self> {
        Some(Self {
            id: value["id"].as_str()?.into(),
            name: value["name"].as_str()?.into(),
            working_directory: value["working_directory"].as_str().unwrap_or("").into(),
        })
    }
}
#[derive(Clone, Copy, PartialEq, Eq)]
struct Size {
    cols: u16,
    rows: u16,
    pixel_width: u16,
    pixel_height: u16,
}
impl Size {
    fn new(cols: u16, rows: u16) -> Self {
        Self {
            cols,
            rows,
            pixel_width: cols.saturating_mul(10),
            pixel_height: rows.saturating_mul(23),
        }
    }
    fn json(self) -> Value {
        json!({"cols":self.cols,"rows":self.rows,"pixel_width":self.pixel_width,"pixel_height":self.pixel_height})
    }
}
impl Dimensions for Size {
    fn total_lines(&self) -> usize {
        self.rows as usize
    }
    fn screen_lines(&self) -> usize {
        self.rows as usize
    }
    fn columns(&self) -> usize {
        self.cols as usize
    }
}
#[derive(Clone)]
struct Listener(SyncSender<TerminalEvent>);
impl EventListener for Listener {
    fn send_event(&self, event: TerminalEvent) {
        let _ = self.0.try_send(event);
    }
}

enum Command {
    Active(bool),
    List,
    Select(String),
    Create(String, String),
    Rename(String),
    Close,
    Detach,
    Input(Vec<u8>),
    Response(Vec<u8>),
    Resize(Size),
    Scroll(i32),
}
enum NetworkEvent {
    Sessions(Vec<Session>),
    Selected(String),
    Attached(String),
    Output(String, Vec<u8>),
    Detached,
    Disconnected(String),
    Error(String),
}
#[derive(Clone, PartialEq, Eq)]
struct Endpoint {
    host: String,
    port: u16,
    token: String,
}
impl Endpoint {
    fn request(&self, method: &str, path: &str, body: &[u8]) -> Result<Vec<u8>, String> {
        crate::window_picker::request(
            &self.host,
            self.port,
            &self.token,
            method,
            path,
            "application/json",
            body,
        )
        .map(|(_, body)| body)
    }
    fn json(&self, method: &str, path: &str, value: Value) -> Result<Value, String> {
        let body = self.request(
            method,
            path,
            &serde_json::to_vec(&value).map_err(|e| e.to_string())?,
        )?;
        if body.is_empty() {
            Ok(Value::Null)
        } else {
            serde_json::from_slice(&body).map_err(|e| e.to_string())
        }
    }
}
struct Connection {
    id: String,
    client: String,
    cursor: u64,
}
impl Connection {
    fn path(&self, action: &str) -> String {
        format!("/terminal/{}/{action}?client={}", self.id, self.client)
    }
}

fn network_worker(endpoint: Endpoint, rx: Receiver<Command>, tx: SyncSender<NetworkEvent>) {
    let mut active = false;
    let mut selected: Option<String> = None;
    let mut connection: Option<Connection> = None;
    let mut size = Size::new(80, 24);
    let mut retry_at = Instant::now();
    let detach = |connection: &mut Option<Connection>| {
        if let Some(connection) = connection.take() {
            let _ = endpoint.request("POST", &connection.path("detach"), &[]);
        }
    };
    let report = |error: String| {
        let _ = tx.try_send(NetworkEvent::Error(error));
    };
    loop {
        match rx.recv_timeout(Duration::from_millis(if active { 50 } else { 200 })) {
            Ok(command) => {
                let result: Result<(), String> = (|| {
                    match command {
                        Command::Active(value) => {
                            active = value;
                            if !active {
                                detach(&mut connection);
                            }
                            retry_at = Instant::now();
                        }
                        Command::List => {
                            let values = endpoint.json("GET", "/terminal/sessions", Value::Null)?;
                            let sessions = values
                                .as_array()
                                .ok_or("invalid terminal session list")?
                                .iter()
                                .filter_map(Session::parse)
                                .collect();
                            let _ = tx.try_send(NetworkEvent::Sessions(sessions));
                        }
                        Command::Select(id) => {
                            detach(&mut connection);
                            selected = Some(id);
                            retry_at = Instant::now();
                        }
                        Command::Create(name, directory) => {
                            let value = endpoint.json(
                                "POST",
                                "/terminal/sessions",
                                json!({"name":name,"working_directory":directory}),
                            )?;
                            let session =
                                Session::parse(&value).ok_or("invalid created terminal")?;
                            detach(&mut connection);
                            selected = Some(session.id.clone());
                            let _ = tx.try_send(NetworkEvent::Selected(session.id));
                            let values = endpoint.json("GET", "/terminal/sessions", Value::Null)?;
                            let sessions = values
                                .as_array()
                                .ok_or("invalid terminal session list")?
                                .iter()
                                .filter_map(Session::parse)
                                .collect();
                            let _ = tx.try_send(NetworkEvent::Sessions(sessions));
                            retry_at = Instant::now();
                        }
                        Command::Rename(name) => {
                            if let Some(id) = &selected {
                                endpoint.json(
                                    "POST",
                                    &format!("/terminal/{id}/rename"),
                                    json!({"name":name}),
                                )?;
                            }
                            let values = endpoint.json("GET", "/terminal/sessions", Value::Null)?;
                            let sessions = values
                                .as_array()
                                .ok_or("invalid terminal session list")?
                                .iter()
                                .filter_map(Session::parse)
                                .collect();
                            let _ = tx.try_send(NetworkEvent::Sessions(sessions));
                        }
                        Command::Close => {
                            detach(&mut connection);
                            if let Some(id) = selected.take() {
                                endpoint.request("DELETE", &format!("/terminal/{id}"), &[])?;
                            }
                            let _ = tx.try_send(NetworkEvent::Detached);
                            let values = endpoint.json("GET", "/terminal/sessions", Value::Null)?;
                            let sessions = values
                                .as_array()
                                .ok_or("invalid terminal session list")?
                                .iter()
                                .filter_map(Session::parse)
                                .collect();
                            let _ = tx.try_send(NetworkEvent::Sessions(sessions));
                        }
                        Command::Detach => {
                            detach(&mut connection);
                            selected = None;
                            let _ = tx.try_send(NetworkEvent::Detached);
                        }
                        Command::Input(bytes) => {
                            if let Some(connection) = &connection {
                                // Keep protocol boundaries intact; fail visibly rather than queue forever.
                                for bytes in bytes.chunks(64 * 1024) {
                                    endpoint.request("POST", &connection.path("input"), bytes)?;
                                }
                            } else {
                                return Err("Terminal reconnecting; input was not sent".into());
                            }
                        }
                        Command::Response(bytes) => {
                            if let Some(connection) = &connection {
                                endpoint.request("POST", &connection.path("response"), &bytes)?;
                            }
                        }
                        Command::Resize(value) => {
                            size = value;
                            if let Some(connection) = &connection {
                                endpoint.json("POST", &connection.path("resize"), size.json())?;
                            }
                        }
                        Command::Scroll(lines) => {
                            if let Some(connection) = &connection {
                                endpoint.json(
                                    "POST",
                                    &connection.path("scroll"),
                                    json!({"lines":lines}),
                                )?;
                            }
                        }
                    }
                    Ok(())
                })();
                if let Err(error) = result {
                    report(error);
                }
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                detach(&mut connection);
                break;
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
        }
        if !active || selected.is_none() || Instant::now() < retry_at {
            continue;
        }
        if connection.is_none() {
            let id = selected.as_ref().unwrap();
            match endpoint.json("POST", &format!("/terminal/{id}/attach"), size.json()) {
                Ok(value) => {
                    if let Some(client) = value["client"].as_str() {
                        // Deliver the reset before consuming any bytes from the new attachment.
                        if tx.send(NetworkEvent::Attached(id.clone())).is_err() {
                            break;
                        }
                        connection = Some(Connection {
                            id: id.clone(),
                            client: client.into(),
                            cursor: 0,
                        });
                    } else {
                        report("invalid terminal attachment".into());
                        retry_at = Instant::now() + Duration::from_secs(2);
                    }
                }
                Err(error) => {
                    let _ = tx.try_send(NetworkEvent::Disconnected(error));
                    retry_at = Instant::now() + Duration::from_secs(2);
                }
            }
        }
        if let Some(current) = connection.as_mut() {
            match endpoint.json(
                "GET",
                &format!("{}&cursor={}", current.path("output"), current.cursor),
                Value::Null,
            ) {
                Ok(value) => {
                    if value["reset"].as_bool().unwrap_or(false)
                        || value["ended"].as_bool().unwrap_or(false)
                    {
                        detach(&mut connection);
                        retry_at = Instant::now() + Duration::from_millis(100);
                    } else {
                        let bytes: Vec<u8> = value["bytes"]
                            .as_array()
                            .into_iter()
                            .flatten()
                            .filter_map(|v| v.as_u64().and_then(|n| u8::try_from(n).ok()))
                            .collect();
                        if bytes.is_empty()
                            || tx
                                .try_send(NetworkEvent::Output(current.id.clone(), bytes))
                                .is_ok()
                        {
                            current.cursor = value["cursor"].as_u64().unwrap_or(current.cursor);
                        }
                        // If the UI stalls, leave the cursor at the last delivered output.
                        // The host's bounded ring requests a redraw if it rolls past us.
                    }
                }
                Err(error) => {
                    let _ = tx.try_send(NetworkEvent::Disconnected(error));
                    detach(&mut connection);
                    retry_at = Instant::now() + Duration::from_secs(2);
                }
            }
        }
    }
}

pub struct TerminalPanel {
    endpoint: Option<Endpoint>,
    command: Option<SyncSender<Command>>,
    network: Option<Receiver<NetworkEvent>>,
    terminal: Term<Listener>,
    processor: Processor,
    listener: Listener,
    events: Receiver<TerminalEvent>,
    sessions: Vec<Session>,
    selected: Option<String>,
    active: bool,
    attached: bool,
    size: Size,
    font_size: f32,
    pub control_alignment: ControlAlignment,
    status: String,
    title: String,
    create_open: bool,
    rename_open: bool,
    new_name: String,
    new_directory: String,
    ctrl: bool,
    alt: bool,
    shift: bool,
    interaction: bool,
    typing: bool,
    ime_buffer: String,
    ime_preedit: bool,
    ime_area: Option<(Rect, Rect)>,
    selection_drag: bool,
    gesture_scroll: f32,
    last_refresh: Instant,
    auto_select: bool,
}
impl Default for TerminalPanel {
    fn default() -> Self {
        let (tx, events) = mpsc::sync_channel(256);
        let listener = Listener(tx);
        let size = Size::new(80, 24);
        Self {
            endpoint: None,
            command: None,
            network: None,
            terminal: Term::new(Config::default(), &size, listener.clone()),
            processor: Processor::new(),
            listener,
            events,
            sessions: Vec::new(),
            selected: None,
            active: false,
            attached: false,
            size,
            font_size: 18.,
            control_alignment: ControlAlignment::Center,
            status: "Select or create a terminal session".into(),
            title: String::new(),
            create_open: false,
            rename_open: false,
            new_name: String::new(),
            new_directory: String::new(),
            ctrl: false,
            alt: false,
            shift: false,
            interaction: false,
            typing: false,
            ime_buffer: String::new(),
            ime_preedit: false,
            ime_area: None,
            selection_drag: false,
            gesture_scroll: 0.,
            last_refresh: Instant::now() - Duration::from_secs(10),
            auto_select: true,
        }
    }
}
impl TerminalPanel {
    pub fn font_size(&self) -> f32 {
        self.font_size
    }

    pub fn set_font_size(&mut self, size: f32) {
        self.font_size = size.clamp(10.0, 28.0);
    }

    pub fn configure(&mut self, host: String, port: u16, token: String) {
        let endpoint = Endpoint { host, port, token };
        if self.endpoint.as_ref() == Some(&endpoint) {
            return;
        }
        let (tx, rx) = mpsc::sync_channel(64);
        let (events_tx, events_rx) = mpsc::sync_channel(16);
        self.command = Some(tx);
        self.network = Some(events_rx);
        self.endpoint = Some(endpoint.clone());
        self.sessions.clear();
        self.selected = None;
        self.attached = false;
        self.auto_select = true;
        self.reset();
        self.typing = false;
        self.ime_area = None;
        self.title.clear();
        self.status = "Select or create a terminal session".into();
        thread::spawn(move || network_worker(endpoint, rx, events_tx));
        self.send(Command::Active(self.active));
        self.send(Command::List);
    }
    pub fn set_active(&mut self, active: bool) {
        if self.active == active {
            return;
        }
        self.active = active;
        self.attached = false;
        if !active {
            self.reset_modifiers();
            self.typing = false;
            self.ime_preedit = false;
            self.ime_buffer.clear();
        }
        self.send(Command::Active(active));
        if active {
            self.send(Command::List);
        }
    }
    fn send(&mut self, command: Command) {
        if let Some(tx) = &self.command {
            if let Err(error) = tx.try_send(command) {
                self.status = match error {
                    TrySendError::Full(_) => "Terminal busy; input queue is full".into(),
                    TrySendError::Disconnected(_) => "Terminal connection stopped".into(),
                };
            }
        }
    }
    fn reset(&mut self) {
        self.reset_modifiers();
        self.terminal = Term::new(Config::default(), &self.size, self.listener.clone());
        self.processor = Processor::new();
        self.ime_buffer.clear();
        self.ime_preedit = false;
    }
    fn reset_modifiers(&mut self) {
        self.ctrl = false;
        self.alt = false;
        self.shift = false;
    }
    pub fn paste(&mut self, text: &str) {
        if !self.attached {
            self.status = "Attach a terminal before pasting".into();
            return;
        }
        // Bracketed paste must not be terminated by pasted control characters.
        let text = text.replace('\u{1b}', "");
        let bytes = if self.terminal.mode().contains(TermMode::BRACKETED_PASTE) {
            format!("\x1b[200~{text}\x1b[201~").into_bytes()
        } else {
            text.replace("\r\n", "\n").replace('\n', "\r").into_bytes()
        };
        self.send(Command::Input(bytes));
        self.reset_modifiers();
    }
    pub fn insert_path(&mut self, path: &str) {
        self.paste(&format!("'{}' ", path.replace('\'', "'\\''")));
    }
    pub fn working_directory(&self) -> String {
        self.sessions
            .iter()
            .find(|s| Some(&s.id) == self.selected.as_ref())
            .map(|s| s.working_directory.clone())
            .unwrap_or_default()
    }
    fn text(&mut self, text: &str) {
        if text.is_empty() {
            return;
        }
        let text = if self.shift {
            text.chars().map(shift_character).collect::<String>()
        } else {
            text.to_owned()
        };
        let mut bytes = Vec::new();
        if self.alt {
            bytes.push(0x1b);
        }
        if self.ctrl {
            for c in text.chars() {
                if let Some(code) = control_character(c) {
                    bytes.push(code);
                }
            }
        } else {
            bytes.extend(text.replace('\n', "\r").as_bytes());
        }
        self.reset_modifiers();
        self.terminal.scroll_display(Scroll::Bottom);
        self.terminal.selection = None;
        if !bytes.is_empty() {
            self.send(Command::Input(bytes));
        }
    }
    fn process_network(&mut self) {
        while let Some(event) = self.network.as_ref().and_then(|rx| rx.try_recv().ok()) {
            match event {
                NetworkEvent::Sessions(sessions) => {
                    self.sessions = sessions;
                    if self.selected.is_none() && self.auto_select && !self.sessions.is_empty() {
                        let id = self.sessions[0].id.clone();
                        self.selected = Some(id.clone());
                        self.send(Command::Select(id));
                        self.auto_select = false;
                    }
                }
                NetworkEvent::Selected(id) => {
                    self.reset_modifiers();
                    self.selected = Some(id);
                    self.attached = false;
                    self.create_open = false;
                }
                NetworkEvent::Attached(id) if self.selected.as_ref() == Some(&id) => {
                    self.reset();
                    self.attached = true;
                    self.status = "Connected".into();
                }
                NetworkEvent::Output(id, bytes) if self.selected.as_ref() == Some(&id) => {
                    self.processor.advance(&mut self.terminal, &bytes);
                }
                NetworkEvent::Detached => {
                    self.reset_modifiers();
                    self.selected = None;
                    self.attached = false;
                    self.typing = false;
                    self.auto_select = false;
                    self.status = "Detached; shell continues on the computer".into();
                }
                NetworkEvent::Error(error) => self.status = error,
                NetworkEvent::Disconnected(error) => {
                    self.reset_modifiers();
                    self.attached = false;
                    self.status = error;
                }
                _ => {}
            }
        }
    }
    fn process_terminal_events(
        &mut self,
        ui: &egui::Ui,
        actions: &mut Vec<TerminalAction>,
        cell: Vec2,
    ) {
        while let Ok(event) = self.events.try_recv() {
            match event {
                TerminalEvent::PtyWrite(text) => self.send(Command::Response(text.into_bytes())),
                TerminalEvent::Title(title) => self.title = title,
                TerminalEvent::ResetTitle => self.title.clear(),
                TerminalEvent::ClipboardStore(_, text) => actions.push(TerminalAction::Copy(text)),
                TerminalEvent::ColorRequest(index, formatter) => {
                    let color = self.terminal.colors()[index].unwrap_or_else(|| {
                        let color = indexed_color(index, ui);
                        Rgb {
                            r: color.r(),
                            g: color.g(),
                            b: color.b(),
                        }
                    });
                    self.send(Command::Response(formatter(color).into_bytes()));
                }
                TerminalEvent::TextAreaSizeRequest(formatter) => self.send(Command::Response(
                    formatter(WindowSize {
                        num_lines: self.size.rows,
                        num_cols: self.size.cols,
                        cell_width: cell.x as u16,
                        cell_height: cell.y as u16,
                    })
                    .into_bytes(),
                )),
                _ => {}
            }
        }
    }
    pub fn ui(&mut self, ui: &mut egui::Ui) -> Vec<TerminalAction> {
        let mut actions = Vec::new();
        self.process_network();
        if self.active && self.last_refresh.elapsed() > Duration::from_secs(5) {
            self.send(Command::List);
            self.last_refresh = Instant::now();
        }
        let mut switch = None;
        ui.scope(|ui| {
            ui.spacing_mut().button_padding = Vec2::new(8., 6.);
            ui.spacing_mut().interact_size = Vec2::new(32., 40.);
            ui.spacing_mut().item_spacing.x = 6.;
            ui.horizontal(|ui| {
                if crate::icons::button(ui, crate::icons::Icon::Back, "Back", true, false).clicked()
                {
                    self.typing = false;
                    actions.push(TerminalAction::Back);
                }
                let current = self
                    .sessions
                    .iter()
                    .find(|s| Some(&s.id) == self.selected.as_ref())
                    .map(|s| s.name.as_str())
                    .unwrap_or("Sessions");
                let dropdown_width = (ui.available_width() - 88.).max(64.);
                ui.allocate_ui(Vec2::new(dropdown_width, 40.), |ui| {
                    egui::ComboBox::from_id_salt("terminal-sessions")
                        .selected_text(current)
                        .width(dropdown_width)
                        .truncate()
                        .show_ui(ui, |ui| {
                            for session in &self.sessions {
                                if ui
                                    .selectable_label(
                                        Some(&session.id) == self.selected.as_ref(),
                                        &session.name,
                                    )
                                    .clicked()
                                {
                                    switch = Some(session.id.clone());
                                }
                            }
                        });
                });
                if ui
                    .add_sized([32., 40.], egui::Button::new("+"))
                    .on_hover_text("New session")
                    .clicked()
                {
                    self.create_open = !self.create_open;
                    self.rename_open = false;
                    self.typing = false;
                    self.new_name = format!("shell-{}", self.sessions.len() + 1);
                    self.new_directory.clear(); // Empty means the host user's home directory.
                }
                crate::icons::menu(ui, crate::icons::Icon::More, "More", |ui| {
                    if ui
                        .add_enabled(self.attached, egui::Button::new("Attach file"))
                        .clicked()
                    {
                        actions.push(TerminalAction::AttachFile {
                            working_directory: self.working_directory(),
                        });
                        ui.close();
                    }
                    if ui.button("Refresh sessions").clicked() {
                        self.send(Command::List);
                        ui.close();
                    }
                    if ui
                        .add_enabled(self.selected.is_some(), egui::Button::new("Rename"))
                        .clicked()
                    {
                        self.rename_open = true;
                        self.create_open = false;
                        self.typing = false;
                        self.new_name = self
                            .sessions
                            .iter()
                            .find(|s| Some(&s.id) == self.selected.as_ref())
                            .map(|s| s.name.clone())
                            .unwrap_or_default();
                        ui.close();
                    }
                    if ui
                        .add_enabled(self.selected.is_some(), egui::Button::new("Detach"))
                        .clicked()
                    {
                        self.send(Command::Detach);
                        self.reset_modifiers();
                        self.attached = false;
                        self.typing = false;
                        ui.close();
                    }
                    if ui
                        .add_enabled(self.selected.is_some(), egui::Button::new("Close shell"))
                        .clicked()
                    {
                        self.send(Command::Close);
                        self.reset_modifiers();
                        self.attached = false;
                        self.typing = false;
                        ui.close();
                    }
                    ui.separator();
                    ui.menu_button("Control alignment", |ui| {
                        for alignment in [
                            ControlAlignment::Left,
                            ControlAlignment::Center,
                            ControlAlignment::Right,
                        ] {
                            if ui
                                .selectable_value(
                                    &mut self.control_alignment,
                                    alignment,
                                    alignment.label(),
                                )
                                .clicked()
                            {
                                ui.close();
                            }
                        }
                    });
                    ui.checkbox(&mut self.interaction, "Send mouse to application");
                    ui.label("Long press selects text");
                    ui.add(egui::Slider::new(&mut self.font_size, 10.0..=28.0).text("Font"));
                    if let Some(text) = self.terminal.selection_to_string() {
                        if ui.button("Copy selection").clicked() {
                            actions.push(TerminalAction::Copy(text));
                            ui.close();
                        }
                    }
                });
            });
        });
        if let Some(id) = switch {
            self.reset_modifiers();
            self.selected = Some(id.clone());
            self.attached = false;
            self.auto_select = false;
            self.send(Command::Select(id));
        }
        if self.create_open || self.rename_open {
            ui.label("Name");
            ui.add(
                egui::TextEdit::singleline(&mut self.new_name).desired_width(ui.available_width()),
            );
            if self.create_open {
                ui.label("Folder");
                ui.add(
                    egui::TextEdit::singleline(&mut self.new_directory)
                        .desired_width(ui.available_width())
                        .hint_text("Home"),
                );
            }
            ui.horizontal(|ui| {
                if ui
                    .button(if self.create_open { "Create" } else { "Rename" })
                    .clicked()
                {
                    if self.create_open {
                        self.send(Command::Create(
                            self.new_name.clone(),
                            self.new_directory.clone(),
                        ));
                    } else {
                        self.send(Command::Rename(self.new_name.clone()));
                        self.rename_open = false;
                    }
                }
                if ui.button("Cancel").clicked() {
                    self.create_open = false;
                    self.rename_open = false;
                }
            });
        }
        if !self.attached || self.status != "Connected" {
            ui.label(&self.status);
        }
        let font = FontId::monospace(self.font_size);
        let cell =
            ui.fonts_mut(|fonts| Vec2::new(fonts.glyph_width(&font, 'M'), fonts.row_height(&font)));
        let mut available = ui.available_size();
        available.y = (available.y - 92. - ui.spacing().item_spacing.y).max(cell.y);
        let available = available.max(Vec2::new(cell.x * 2., cell.y));
        let (rect, response) = ui.allocate_exact_size(available, egui::Sense::click_and_drag());
        let cols = ((rect.width() / cell.x) as u16).clamp(2, 500);
        let rows = ((rect.height() / cell.y) as u16).clamp(1, 300);
        // tmux otherwise queries both sizes on every SIGWINCH. Supplying
        // TIOCGWINSZ pixels avoids delayed query replies becoming shell keys.
        let scale = ui.ctx().pixels_per_point();
        let size = Size {
            cols,
            rows,
            pixel_width: (cols as f32 * cell.x * scale)
                .round()
                .clamp(cols as f32, u16::MAX as f32) as u16,
            pixel_height: (rows as f32 * cell.y * scale)
                .round()
                .clamp(rows as f32, u16::MAX as f32) as u16,
        };
        if size != self.size {
            self.size = size;
            self.terminal.resize(size);
            self.send(Command::Resize(size));
        }
        self.process_terminal_events(ui, &mut actions, cell);
        let pointer_point = |position: Pos2, offset: usize| {
            Point::new(
                Line(
                    ((position.y - rect.top()) / cell.y)
                        .floor()
                        .clamp(0., size.rows as f32 - 1.) as i32
                        - offset as i32,
                ),
                Column(
                    ((position.x - rect.left()) / cell.x)
                        .floor()
                        .clamp(0., size.cols as f32 - 1.) as usize,
                ),
            )
        };
        let offset = self.terminal.grid().display_offset();
        if response.long_touched() {
            if let Some(position) = response.interact_pointer_pos() {
                self.terminal.selection = Some(Selection::new(
                    SelectionType::Semantic,
                    pointer_point(position, offset),
                    Side::Left,
                ));
                self.selection_drag = true;
                self.typing = false;
            }
        }
        if response.dragged() {
            if self.selection_drag {
                if let Some(position) = response.interact_pointer_pos() {
                    if let Some(selection) = self.terminal.selection.as_mut() {
                        selection.update(pointer_point(position, offset), Side::Right);
                    }
                }
            } else {
                self.gesture_scroll += ui.input(|input| input.pointer.delta().y);
                let lines = (self.gesture_scroll / cell.y) as i32;
                if lines != 0 {
                    self.scroll(
                        lines,
                        response.interact_pointer_pos().unwrap_or(rect.center()),
                        rect,
                        cell,
                    );
                    self.gesture_scroll -= lines as f32 * cell.y;
                }
            }
        }
        if response.drag_stopped() {
            self.selection_drag = false;
            self.gesture_scroll = 0.;
        }
        if response.clicked() && self.attached {
            self.typing = true;
            self.terminal.selection = None;
            actions.push(TerminalAction::ShowKeyboard);
            if self.interaction {
                if let Some(position) = response.interact_pointer_pos() {
                    self.mouse(0, position, rect, cell, false);
                    self.mouse(0, position, rect, cell, true);
                }
            }
        }
        if response.hovered() {
            let scroll = ui.input(|input| input.smooth_scroll_delta.y);
            if scroll.abs() > 0.1 {
                self.scroll(
                    (scroll / cell.y).round().clamp(-10., 10.) as i32,
                    response.hover_pos().unwrap_or(rect.center()),
                    rect,
                    cell,
                );
            }
            let zoom = ui.input(|input| input.zoom_delta());
            if (zoom - 1.).abs() > 0.005 {
                self.font_size = (self.font_size * zoom).clamp(10., 28.);
            }
        }
        if let Some(text) = self.terminal.selection_to_string() {
            response.context_menu(|ui| {
                if ui.button("Copy").clicked() {
                    actions.push(TerminalAction::Copy(text));
                    ui.close();
                }
            });
        }
        self.paint(ui, rect, cell, font);
        self.ime(ui, rect, cell, &mut actions);
        self.controls(ui, &mut actions);
        ui.ctx().request_repaint_after(Duration::from_millis(40));
        actions
    }
    fn controls(&mut self, ui: &mut egui::Ui, actions: &mut Vec<TerminalAction>) {
        use crate::icons::{self, Icon};
        ui.scope(|ui| {
            ui.spacing_mut().button_padding = Vec2::new(4., 6.);
            ui.spacing_mut().interact_size = Vec2::new(44., 44.);
            ui.spacing_mut().item_spacing = Vec2::splat(4.);
            aligned_control_row(
                ui,
                self.control_alignment,
                &["Ctrl", "Alt", "Shift", "Fn"],
                2,
                |ui| {
                    if crate::keyboard::paste_button(ui, self.attached).clicked() {
                        actions.push(TerminalAction::Paste);
                    }
                    for (label, selected) in [
                        ("Ctrl", &mut self.ctrl),
                        ("Alt", &mut self.alt),
                        ("Shift", &mut self.shift),
                    ] {
                        if ui
                            .add_sized([44., 44.], egui::Button::new(label).selected(*selected))
                            .clicked()
                        {
                            *selected = !*selected;
                        }
                    }
                    let functions = ui
                        .add_sized([44., 44.], egui::Button::new("Fn"))
                        .on_hover_text("Function keys");
                    egui::Popup::menu(&functions).show(|ui| {
                        egui::Grid::new("terminal_function_keys")
                            .spacing(Vec2::splat(4.))
                            .show(ui, |ui| {
                                for (i, key) in [
                                    egui::Key::F1,
                                    egui::Key::F2,
                                    egui::Key::F3,
                                    egui::Key::F4,
                                    egui::Key::F5,
                                    egui::Key::F6,
                                    egui::Key::F7,
                                    egui::Key::F8,
                                    egui::Key::F9,
                                    egui::Key::F10,
                                    egui::Key::F11,
                                    egui::Key::F12,
                                ]
                                .iter()
                                .enumerate()
                                {
                                    if ui
                                        .add_sized(
                                            [52., 44.],
                                            egui::Button::new(format!("F{}", i + 1)),
                                        )
                                        .clicked()
                                    {
                                        self.key(*key, egui::Modifiers::NONE);
                                        ui.close();
                                    }
                                    if i % 3 == 2 {
                                        ui.end_row();
                                    }
                                }
                            });
                    });
                    if icons::button(
                        ui,
                        Icon::Keyboard,
                        if self.typing {
                            "Hide keyboard"
                        } else {
                            "Show keyboard"
                        },
                        self.attached,
                        self.typing,
                    )
                    .clicked()
                    {
                        self.typing = !self.typing;
                        if self.typing {
                            actions.push(TerminalAction::ShowKeyboard);
                        }
                    }
                },
            );
            aligned_control_row(
                ui,
                self.control_alignment,
                &["Esc", "Tab", "←", "↑", "↓", "→"],
                1,
                |ui| {
                    for (label, key) in [
                        ("Esc", egui::Key::Escape),
                        ("Tab", egui::Key::Tab),
                        ("←", egui::Key::ArrowLeft),
                        ("↑", egui::Key::ArrowUp),
                        ("↓", egui::Key::ArrowDown),
                        ("→", egui::Key::ArrowRight),
                    ] {
                        if ui.add_sized([44., 44.], egui::Button::new(label)).clicked() {
                            self.key(key, egui::Modifiers::NONE);
                        }
                    }
                    if icons::button(ui, Icon::Enter, "Enter", self.attached, false).clicked() {
                        self.key(egui::Key::Enter, egui::Modifiers::NONE);
                    }
                },
            );
        });
    }
    fn key(&mut self, key: egui::Key, mut modifiers: egui::Modifiers) {
        modifiers.ctrl |= self.ctrl;
        modifiers.alt |= self.alt;
        modifiers.shift |= self.shift;
        if let Some(bytes) = key_bytes(key, modifiers, *self.terminal.mode()) {
            self.send(Command::Input(bytes));
            self.reset_modifiers();
        }
    }
    pub fn maintain_ime(&self, ctx: &egui::Context) {
        if self.active && self.typing && self.attached && ctx.input(|input| input.focused) {
            if let Some((rect, cursor_rect)) = self.ime_area {
                ctx.output_mut(|output| {
                    output.ime = Some(egui::output::IMEOutput {
                        purpose: egui::IMEPurpose::Terminal,
                        rect,
                        cursor_rect,
                        should_interrupt_composition: false,
                    });
                });
            }
        }
    }
    fn ime(
        &mut self,
        ui: &mut egui::Ui,
        rect: Rect,
        cell: Vec2,
        actions: &mut Vec<TerminalAction>,
    ) {
        let id = egui::Id::new("terminal-native-ime");
        if !self.typing || !self.attached || self.create_open || self.rename_open {
            self.ime_area = None;
            ui.memory_mut(|memory| {
                if memory.has_focus(id) {
                    memory.surrender_focus(id);
                }
            });
            return;
        }
        let incoming = ui.input(|input| input.events.clone());
        // The transparent one-pixel editor provides native IME composition state.
        // The terminal grid is the sole visible input surface.
        let mut editor_ui = ui.new_child(
            egui::UiBuilder::new().max_rect(Rect::from_min_size(rect.min, Vec2::new(1., 1.))),
        );
        editor_ui.set_clip_rect(Rect::NOTHING);
        let response = editor_ui.add(
            egui::TextEdit::multiline(&mut self.ime_buffer)
                .id(id)
                .font(FontId::monospace(1.))
                .text_color(Color32::TRANSPARENT)
                .frame(egui::Frame::NONE)
                .margin(0.)
                .desired_width(1.)
                .desired_rows(1),
        );
        // Requesting focus also interrupts native IME composition in egui 0.36.
        // Keep the existing focus through idle frames and composition updates.
        if !response.has_focus() {
            response.request_focus();
        }
        let content = self.terminal.renderable_content();
        let cursor = content.cursor.point;
        let cursor_position = rect.min
            + Vec2::new(
                cursor.column.0 as f32 * cell.x,
                (cursor.line.0 + content.display_offset as i32).max(0) as f32 * cell.y,
            );
        let cursor_rect = Rect::from_min_size(cursor_position, Vec2::new(1., cell.y));
        self.ime_area = Some((rect, cursor_rect));
        ui.ctx().output_mut(|output| {
            output.ime = Some(egui::output::IMEOutput {
                purpose: egui::IMEPurpose::Terminal,
                rect,
                cursor_rect,
                should_interrupt_composition: false,
            });
        });
        self.consume_input(&incoming, actions);
    }
    fn consume_input(&mut self, incoming: &[egui::Event], actions: &mut Vec<TerminalAction>) {
        let mut commits: Vec<&str> = incoming
            .iter()
            .filter_map(|event| match event {
                egui::Event::Ime(egui::ImeEvent::Commit(text)) => Some(text.as_str()),
                _ => None,
            })
            .collect();
        let composing=self.ime_preedit||incoming.iter().any(|event|matches!(event,egui::Event::Ime(egui::ImeEvent::Preedit{text,..}) if !text.is_empty()));
        let hardware_alt = incoming.iter().any(
            |event| matches!(event,egui::Event::Key{pressed:true,modifiers,..} if modifiers.alt),
        );
        let control_key=incoming.iter().any(|event|matches!(event,egui::Event::Key{key,pressed:true,modifiers,..} if (modifiers.ctrl || self.ctrl) && key.name().len()==1));
        let newline=incoming.iter().any(|event|matches!(event,egui::Event::Text(text)|egui::Event::Ime(egui::ImeEvent::Commit(text)) if text=="\n"||text=="\r"));
        let ime_delete = incoming.iter().any(|event| {
            matches!(
                event,
                egui::Event::Ime(egui::ImeEvent::DeleteSurrounding { .. })
            )
        });
        let mut consumed_text = false;
        for event in incoming {
            match event {
                egui::Event::Text(text) if !composing && !control_key => {
                    if let Some(index) = commits.iter().position(|commit| *commit == text) {
                        commits.remove(index);
                        continue;
                    }
                    self.alt |= hardware_alt;
                    self.text(text);
                    consumed_text = true;
                }
                egui::Event::Ime(egui::ImeEvent::Commit(text)) => {
                    self.text(text);
                    self.ime_preedit = false;
                    consumed_text = true;
                }
                egui::Event::Ime(egui::ImeEvent::Preedit { text, .. }) => {
                    self.ime_preedit = !text.is_empty()
                }
                egui::Event::Ime(egui::ImeEvent::DeleteSurrounding {
                    before_chars,
                    after_chars,
                }) if !self.ime_preedit => {
                    let mut bytes = vec![0x7f; (*before_chars).min(1024)];
                    for _ in 0..(*after_chars).min(1024) {
                        bytes.extend_from_slice(b"\x1b[3~");
                    }
                    if !bytes.is_empty() {
                        self.send(Command::Input(bytes));
                        self.reset_modifiers();
                    }
                }
                egui::Event::Paste(text) => self.paste(text),
                egui::Event::Copy | egui::Event::Cut => {
                    if let Some(text) = self.terminal.selection_to_string() {
                        actions.push(TerminalAction::Copy(text));
                    }
                }
                egui::Event::Key {
                    key,
                    pressed: true,
                    modifiers,
                    ..
                } => {
                    if *key == egui::Key::Enter && newline {
                        continue;
                    }
                    if matches!(key, egui::Key::Backspace | egui::Key::Delete)
                        && (composing || ime_delete)
                    {
                        continue;
                    }
                    if modifiers.ctrl && modifiers.shift && *key == egui::Key::C {
                        if let Some(text) = self.terminal.selection_to_string() {
                            actions.push(TerminalAction::Copy(text));
                        }
                        continue;
                    }
                    if modifiers.ctrl && modifiers.shift && *key == egui::Key::V {
                        actions.push(TerminalAction::Paste);
                        continue;
                    }
                    self.key(*key, *modifiers);
                }
                _ => {}
            }
        }
        if consumed_text && !self.ime_preedit {
            self.ime_buffer.clear();
        }
        if self.ime_buffer.len() > 16 * 1024 && !self.ime_preedit {
            self.ime_buffer.clear();
        }
    }
    fn mouse(&mut self, button: u8, position: Pos2, rect: Rect, cell: Vec2, release: bool) {
        if !self.terminal.mode().intersects(TermMode::MOUSE_MODE) {
            return;
        }
        let col = ((position.x - rect.left()) / cell.x).max(0.) as u16 + 1;
        let row = ((position.y - rect.top()) / cell.y).max(0.) as u16 + 1;
        let bytes = if self.terminal.mode().contains(TermMode::SGR_MOUSE) {
            format!(
                "\x1b[<{button};{col};{row}{}",
                if release { 'm' } else { 'M' }
            )
            .into_bytes()
        } else if col <= 223 && row <= 223 {
            vec![
                0x1b,
                b'[',
                b'M',
                32 + if release { 3 } else { button },
                32 + col as u8,
                32 + row as u8,
            ]
        } else {
            return;
        };
        self.send(Command::Input(bytes));
    }
    fn scroll(&mut self, lines: i32, position: Pos2, rect: Rect, cell: Vec2) {
        if self.interaction && self.terminal.mode().intersects(TermMode::MOUSE_MODE) {
            for _ in 0..lines.unsigned_abs().min(20) {
                self.mouse(if lines > 0 { 64 } else { 65 }, position, rect, cell, false);
            }
        } else if self.interaction && self.terminal.mode().contains(TermMode::ALT_SCREEN) {
            for _ in 0..lines.unsigned_abs().min(20) {
                self.key(
                    if lines > 0 {
                        egui::Key::ArrowUp
                    } else {
                        egui::Key::ArrowDown
                    },
                    egui::Modifiers::NONE,
                );
            }
        } else {
            if self.terminal.mode().contains(TermMode::ALT_SCREEN) {
                self.send(Command::Scroll(lines));
            } else {
                self.terminal.scroll_display(Scroll::Delta(lines));
            }
        }
    }
    fn paint(&self, ui: &egui::Ui, rect: Rect, cell: Vec2, font: FontId) {
        let painter = ui.painter().with_clip_rect(rect);
        let content = self.terminal.renderable_content();
        let background = terminal_color(Color::Named(NamedColor::Background), content.colors, ui);
        painter.rect_filled(rect, 0., background);
        let offset = content.display_offset as i32;
        for indexed in content.display_iter {
            let line = indexed.point.line.0 + offset;
            if line < 0 || line >= self.size.rows as i32 {
                continue;
            }
            let position =
                rect.min + Vec2::new(indexed.point.column.0 as f32 * cell.x, line as f32 * cell.y);
            let cell_rect = Rect::from_min_size(
                position,
                Vec2::new(
                    cell.x
                        * if indexed.flags.contains(Flags::WIDE_CHAR) {
                            2.
                        } else {
                            1.
                        },
                    cell.y,
                ),
            );
            let mut fg = terminal_color(indexed.fg, content.colors, ui);
            let mut bg = terminal_color(indexed.bg, content.colors, ui);
            if indexed.flags.contains(Flags::INVERSE) {
                std::mem::swap(&mut fg, &mut bg);
            }
            if indexed.flags.contains(Flags::DIM) {
                fg = fg.gamma_multiply(0.65);
            }
            if content
                .selection
                .is_some_and(|selection| selection.contains(indexed.point))
            {
                bg = ui.visuals().selection.bg_fill;
                fg = ui.visuals().selection.stroke.color;
            }
            if bg != background {
                painter.rect_filled(cell_rect, 0., bg);
            }
            if !indexed.flags.intersects(
                Flags::WIDE_CHAR_SPACER | Flags::LEADING_WIDE_CHAR_SPACER | Flags::HIDDEN,
            ) {
                let mut text = indexed.c.to_string();
                if let Some(extra) = indexed.zerowidth() {
                    text.extend(extra);
                }
                painter.text(position, egui::Align2::LEFT_TOP, &text, font.clone(), fg);
                if indexed.flags.contains(Flags::BOLD) {
                    painter.text(
                        position + Vec2::new(0.35, 0.),
                        egui::Align2::LEFT_TOP,
                        &text,
                        font.clone(),
                        fg,
                    );
                }
                if indexed.flags.intersects(Flags::ALL_UNDERLINES) {
                    painter.line_segment(
                        [
                            cell_rect.left_bottom() - Vec2::new(0., 2.),
                            cell_rect.right_bottom() - Vec2::new(0., 2.),
                        ],
                        egui::Stroke::new(1., fg),
                    );
                }
                if indexed.flags.contains(Flags::STRIKEOUT) {
                    painter.line_segment(
                        [cell_rect.left_center(), cell_rect.right_center()],
                        egui::Stroke::new(1., fg),
                    );
                }
            }
        }
        let cursor = content.cursor;
        let line = cursor.point.line.0 + offset;
        if line >= 0 && line < self.size.rows as i32 && cursor.shape != CursorShape::Hidden {
            let position =
                rect.min + Vec2::new(cursor.point.column.0 as f32 * cell.x, line as f32 * cell.y);
            let cursor_rect = Rect::from_min_size(position, cell);
            let color = terminal_color(Color::Named(NamedColor::Cursor), content.colors, ui);
            match cursor.shape {
                CursorShape::Beam => {
                    painter.rect_filled(
                        Rect::from_min_size(position, Vec2::new(2., cell.y)),
                        0.,
                        color,
                    );
                }
                CursorShape::Underline => {
                    painter.rect_filled(
                        Rect::from_min_size(
                            cursor_rect.left_bottom() - Vec2::new(0., 2.),
                            Vec2::new(cell.x, 2.),
                        ),
                        0.,
                        color,
                    );
                }
                _ => {
                    painter.rect_stroke(
                        cursor_rect,
                        0.,
                        egui::Stroke::new(1.5, color),
                        egui::StrokeKind::Inside,
                    );
                }
            }
        }
    }
}

fn aligned_control_row(
    ui: &mut egui::Ui,
    alignment: ControlAlignment,
    labels: &[&str],
    icons: usize,
    contents: impl FnOnce(&mut egui::Ui),
) -> egui::InnerResponse<()> {
    let font = egui::TextStyle::Button.resolve(ui.style());
    let padding =
        (ui.spacing().button_padding.x + ui.visuals().widgets.inactive.bg_stroke.width) * 2.;
    let width = icons as f32 * 44.
        + labels
            .iter()
            .map(|label| {
                let text_width = ui
                    .painter()
                    .layout_no_wrap((*label).into(), font.clone(), Color32::WHITE)
                    .size()
                    .x;
                (text_width + padding).max(44.)
            })
            .sum::<f32>()
        + (labels.len() + icons - 1) as f32 * ui.spacing().item_spacing.x;
    let remaining = (ui.available_width() - width).max(0.);
    let offset = match alignment {
        ControlAlignment::Left => 0.,
        ControlAlignment::Center => remaining * 0.5,
        ControlAlignment::Right => remaining,
    };
    ui.horizontal(|ui| {
        if offset > 0. {
            ui.add_space(offset);
        }
        ui.scope(contents);
    })
}

fn shift_character(c: char) -> char {
    match c {
        'a'..='z' => c.to_ascii_uppercase(),
        '1' => '!',
        '2' => '@',
        '3' => '#',
        '4' => '$',
        '5' => '%',
        '6' => '^',
        '7' => '&',
        '8' => '*',
        '9' => '(',
        '0' => ')',
        '-' => '_',
        '=' => '+',
        '[' => '{',
        ']' => '}',
        '\\' => '|',
        ';' => ':',
        '\'' => '"',
        ',' => '<',
        '.' => '>',
        '/' => '?',
        '`' => '~',
        _ => c,
    }
}
fn control_character(c: char) -> Option<u8> {
    match c {
        '@' | ' ' | '`' => Some(0),
        'a'..='z' => Some(c as u8 - b'a' + 1),
        'A'..='Z' => Some(c as u8 - b'A' + 1),
        '[' => Some(27),
        '\\' => Some(28),
        ']' => Some(29),
        '^' => Some(30),
        '_' | '/' => Some(31),
        '?' => Some(127),
        _ => None,
    }
}
fn key_bytes(key: egui::Key, m: egui::Modifiers, mode: TermMode) -> Option<Vec<u8>> {
    let modifier = 1
        + u8::from(m.shift)
        + 2 * u8::from(m.alt)
        + 4 * u8::from(m.ctrl)
        + 8 * u8::from(m.mac_cmd);
    let arrow = match key {
        egui::Key::ArrowUp => Some('A'),
        egui::Key::ArrowDown => Some('B'),
        egui::Key::ArrowRight => Some('C'),
        egui::Key::ArrowLeft => Some('D'),
        egui::Key::Home => Some('H'),
        egui::Key::End => Some('F'),
        _ => None,
    };
    if let Some(letter) = arrow {
        return Some(
            if modifier > 1 {
                format!("\x1b[1;{modifier}{letter}")
            } else if mode.contains(TermMode::APP_CURSOR) {
                format!("\x1bO{letter}")
            } else {
                format!("\x1b[{letter}")
            }
            .into_bytes(),
        );
    }
    let tilde = match key {
        egui::Key::Insert => Some(2),
        egui::Key::Delete => Some(3),
        egui::Key::PageUp => Some(5),
        egui::Key::PageDown => Some(6),
        egui::Key::F5 => Some(15),
        egui::Key::F6 => Some(17),
        egui::Key::F7 => Some(18),
        egui::Key::F8 => Some(19),
        egui::Key::F9 => Some(20),
        egui::Key::F10 => Some(21),
        egui::Key::F11 => Some(23),
        egui::Key::F12 => Some(24),
        _ => None,
    };
    if let Some(number) = tilde {
        return Some(
            if modifier > 1 {
                format!("\x1b[{number};{modifier}~")
            } else {
                format!("\x1b[{number}~")
            }
            .into_bytes(),
        );
    }
    let function = match key {
        egui::Key::F1 => Some('P'),
        egui::Key::F2 => Some('Q'),
        egui::Key::F3 => Some('R'),
        egui::Key::F4 => Some('S'),
        _ => None,
    };
    if let Some(letter) = function {
        return Some(
            if modifier > 1 {
                format!("\x1b[1;{modifier}{letter}")
            } else {
                format!("\x1bO{letter}")
            }
            .into_bytes(),
        );
    }
    let mut bytes = match key {
        egui::Key::Enter => vec![b'\r'],
        egui::Key::Backspace => vec![if m.ctrl { 8 } else { 127 }],
        egui::Key::Escape => vec![27],
        egui::Key::Tab if m.shift => b"\x1b[Z".to_vec(),
        egui::Key::Tab => vec![b'\t'],
        _ => {
            if m.ctrl {
                let text = key.name();
                let mut chars = text.chars();
                let first = chars.next()?;
                if chars.next().is_some() {
                    return None;
                }
                vec![control_character(if m.shift {
                    shift_character(first)
                } else {
                    first
                })?]
            } else {
                return None;
            }
        }
    };
    if m.alt {
        bytes.insert(0, 27);
    }
    Some(bytes)
}
fn terminal_color(color: Color, colors: &Colors, ui: &egui::Ui) -> Color32 {
    match color {
        Color::Spec(rgb) => Color32::from_rgb(rgb.r, rgb.g, rgb.b),
        Color::Named(named) => colors[named]
            .map(|rgb| Color32::from_rgb(rgb.r, rgb.g, rgb.b))
            .unwrap_or_else(|| indexed_color(named as usize, ui)),
        Color::Indexed(index) => colors[index as usize]
            .map(|rgb| Color32::from_rgb(rgb.r, rgb.g, rgb.b))
            .unwrap_or_else(|| indexed_color(index as usize, ui)),
    }
}
fn indexed_color(index: usize, ui: &egui::Ui) -> Color32 {
    let ansi: [u32; 16] = if ui.visuals().dark_mode {
        [
            0x32344a, 0xf7768e, 0x9ece6a, 0xe0af68, 0x7aa2f7, 0xbb9af7, 0x7dcfff, 0xa9b1d6,
            0x565f89, 0xff899d, 0xb9f27c, 0xffc777, 0x89b4fa, 0xcba6f7, 0x89dceb, 0xe0e6fc,
        ]
    } else {
        [
            0x575279, 0xb4637a, 0x286983, 0xa97612, 0x56949f, 0x907aa9, 0x2a8092, 0xe1dbd5,
            0x797593, 0xc15c78, 0x3e877b, 0xb88730, 0x6cabb6, 0xa28bbb, 0x4094a5, 0xfaf4ed,
        ]
    };
    if index < 16 {
        let rgb = ansi[index];
        return Color32::from_rgb((rgb >> 16) as u8, (rgb >> 8) as u8, rgb as u8);
    }
    if index < 232 {
        let n = index - 16;
        let step = |v| if v == 0 { 0 } else { 55 + 40 * v as u8 };
        return Color32::from_rgb(step(n / 36), step(n / 6 % 6), step(n % 6));
    }
    if index < 256 {
        let shade = 8 + 10 * (index - 232) as u8;
        return Color32::from_gray(shade);
    }
    match index {
        256 | 267 | 268 => ui.visuals().text_color(),
        257 => ui.visuals().panel_fill,
        258 => ui.visuals().selection.stroke.color,
        259..=266 => indexed_color(index - 259, ui).gamma_multiply(0.66),
        _ => ui.visuals().text_color(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn terminal_queries_alternate_screen_unicode_and_resize() {
        let (tx, rx) = mpsc::sync_channel(256);
        let size = Size::new(20, 5);
        let mut terminal = Term::new(Config::default(), &size, Listener(tx));
        let mut parser: Processor = Processor::new();
        parser.advance(&mut terminal, b"hello\x1b[31m red\x1b[0m\x1b[6n");
        assert!(
            rx.try_iter().any(
                |event| matches!(event,TerminalEvent::PtyWrite(text) if text.contains("[1;10R"))
            )
        );
        assert_eq!(
            terminal.grid()[Point::new(Line(0), Column(6))].fg,
            Color::Named(NamedColor::Red)
        );
        parser.advance(&mut terminal, "\x1b[?1049h\x1b[2J\x1b[Hλ界".as_bytes());
        assert!(terminal.mode().contains(TermMode::ALT_SCREEN));
        assert_eq!(terminal.grid()[Point::new(Line(0), Column(0))].c, 'λ');
        parser.advance(&mut terminal, b"\x1b[?1049l");
        assert_eq!(terminal.grid()[Point::new(Line(0), Column(0))].c, 'h');
        terminal.resize(Size::new(35, 8));
        assert_eq!(terminal.columns(), 35);
        assert_eq!(terminal.screen_lines(), 8);
    }
    #[test]
    fn query_replies_use_response_transport() {
        let ctx = egui::Context::default();
        let (tx, rx) = mpsc::sync_channel(32);
        let mut panel = TerminalPanel::default();
        panel.command = Some(tx);
        panel
            .processor
            .advance(&mut panel.terminal, b"\x1b[18t\x1b[14t\x1b[6n\x1b]11;?\x07");
        let mut output = ctx.run_ui(egui::RawInput::default(), |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| {
                panel.process_terminal_events(ui, &mut Vec::new(), Vec2::new(10., 23.))
            });
        });
        output.textures_delta.clear();
        let replies: Vec<Vec<u8>> = rx
            .try_iter()
            .map(|command| match command {
                Command::Response(bytes) => bytes,
                _ => panic!("query reply was sent as typed input"),
            })
            .collect();
        assert_eq!(replies.len(), 4);
        assert!(replies.iter().any(|bytes| bytes == b"\x1b[8;24;80t"));
        assert!(replies.iter().any(|bytes| bytes == b"\x1b[4;552;800t"));
        assert_eq!(panel.size.json()["pixel_width"], 800);
    }
    #[test]
    fn application_keys_and_modified_arrows() {
        assert_eq!(
            key_bytes(
                egui::Key::ArrowUp,
                egui::Modifiers::NONE,
                TermMode::APP_CURSOR
            )
            .unwrap(),
            b"\x1bOA"
        );
        assert_eq!(
            key_bytes(egui::Key::ArrowLeft, egui::Modifiers::CTRL, TermMode::NONE).unwrap(),
            b"\x1b[1;5D"
        );
        assert_eq!(
            key_bytes(egui::Key::C, egui::Modifiers::CTRL, TermMode::NONE).unwrap(),
            [3]
        );
        assert_eq!(
            key_bytes(egui::Key::F12, egui::Modifiers::NONE, TermMode::NONE).unwrap(),
            b"\x1b[24~"
        );
    }
    #[test]
    fn committed_ime_text_is_sent_once_and_hardware_shortcuts_are_kept() {
        let (tx, rx) = mpsc::sync_channel(32);
        let mut panel = TerminalPanel::default();
        panel.command = Some(tx);
        panel.attached = true;
        let mut actions = Vec::new();
        panel.consume_input(
            &[
                egui::Event::Ime(egui::ImeEvent::Preedit {
                    text: "に".into(),
                    active_range_chars: None,
                }),
                egui::Event::Text("に".into()),
            ],
            &mut actions,
        );
        assert!(rx.try_recv().is_err());
        panel.consume_input(
            &[
                egui::Event::Ime(egui::ImeEvent::Commit("日本語".into())),
                egui::Event::Text("日本語".into()),
            ],
            &mut actions,
        );
        assert!(matches!(rx.try_recv(),Ok(Command::Input(bytes)) if bytes=="日本語".as_bytes()));
        assert!(rx.try_recv().is_err());
        let key = |key, modifiers| egui::Event::Key {
            key,
            physical_key: Some(key),
            pressed: true,
            repeat: false,
            modifiers,
        };
        panel.consume_input(
            &[
                key(egui::Key::C, egui::Modifiers::CTRL),
                egui::Event::Text("c".into()),
            ],
            &mut actions,
        );
        assert!(matches!(rx.try_recv(),Ok(Command::Input(bytes)) if bytes==[3]));
        assert!(rx.try_recv().is_err());
        panel.consume_input(
            &[
                key(egui::Key::P, egui::Modifiers::ALT),
                egui::Event::Text("p".into()),
            ],
            &mut actions,
        );
        assert!(matches!(rx.try_recv(),Ok(Command::Input(bytes)) if bytes==b"\x1bp"));
        panel.consume_input(
            &[
                key(egui::Key::Enter, egui::Modifiers::NONE),
                egui::Event::Ime(egui::ImeEvent::Commit("\n".into())),
                egui::Event::Text("\n".into()),
            ],
            &mut actions,
        );
        assert!(matches!(rx.try_recv(),Ok(Command::Input(bytes)) if bytes==b"\r"));
        assert!(rx.try_recv().is_err());
    }
    #[test]
    fn shift_controls_apply_once_to_tab_arrows_and_function_keys() {
        let (tx, rx) = mpsc::sync_channel(32);
        let mut panel = TerminalPanel::default();
        panel.command = Some(tx);
        for (key, shifted, plain) in [
            (egui::Key::Tab, b"\x1b[Z".as_slice(), b"\t".as_slice()),
            (egui::Key::ArrowLeft, b"\x1b[1;2D", b"\x1b[D"),
            (egui::Key::F1, b"\x1b[1;2P", b"\x1bOP"),
            (egui::Key::F12, b"\x1b[24;2~", b"\x1b[24~"),
        ] {
            panel.shift = true;
            panel.key(key, egui::Modifiers::NONE);
            assert!(matches!(rx.try_recv(), Ok(Command::Input(bytes)) if bytes == shifted));
            assert!(!panel.shift);
            panel.key(key, egui::Modifiers::NONE);
            assert!(matches!(rx.try_recv(), Ok(Command::Input(bytes)) if bytes == plain));
        }
        panel.shift = true;
        panel.ctrl = true;
        panel.alt = true;
        panel.key(egui::Key::ArrowUp, egui::Modifiers::NONE);
        assert!(matches!(rx.try_recv(), Ok(Command::Input(bytes)) if bytes == b"\x1b[1;8A"));
        assert!(!panel.shift && !panel.ctrl && !panel.alt);
    }
    #[test]
    fn shift_waits_for_ime_commit_and_preserves_non_ascii_text() {
        let (tx, rx) = mpsc::sync_channel(32);
        let mut panel = TerminalPanel::default();
        panel.command = Some(tx);
        panel.shift = true;
        let mut actions = Vec::new();
        panel.consume_input(
            &[egui::Event::Ime(egui::ImeEvent::Preedit {
                text: "a".into(),
                active_range_chars: None,
            })],
            &mut actions,
        );
        assert!(panel.shift);
        assert!(rx.try_recv().is_err());
        let text = "a1-=[]\\;',./`日本語λ";
        panel.consume_input(
            &[
                egui::Event::Ime(egui::ImeEvent::Commit(text.into())),
                egui::Event::Text(text.into()),
            ],
            &mut actions,
        );
        assert!(
            matches!(rx.try_recv(), Ok(Command::Input(bytes)) if bytes == "A!_+{}|:\"<>?~日本語λ".as_bytes())
        );
        assert!(rx.try_recv().is_err());
        assert!(!panel.shift);
        panel.consume_input(&[egui::Event::Text("a1".into())], &mut actions);
        assert!(matches!(rx.try_recv(), Ok(Command::Input(bytes)) if bytes == b"a1"));
        panel.shift = true;
        panel.ctrl = true;
        panel.consume_input(
            &[
                egui::Event::Key {
                    key: egui::Key::Num6,
                    physical_key: Some(egui::Key::Num6),
                    pressed: true,
                    repeat: false,
                    modifiers: egui::Modifiers::NONE,
                },
                egui::Event::Text("6".into()),
            ],
            &mut actions,
        );
        assert!(matches!(rx.try_recv(), Ok(Command::Input(bytes)) if bytes == [30]));
        assert!(rx.try_recv().is_err());
    }
    #[test]
    fn terminal_reset_and_session_change_clear_one_shot_modifiers() {
        let mut panel = TerminalPanel::default();
        panel.shift = true;
        panel.ctrl = true;
        panel.alt = true;
        panel.reset();
        assert!(!panel.shift && !panel.ctrl && !panel.alt);
        let (tx, rx) = mpsc::sync_channel(16);
        panel.network = Some(rx);
        panel.shift = true;
        panel.ctrl = true;
        panel.alt = true;
        tx.send(NetworkEvent::Selected("$1".into())).unwrap();
        panel.process_network();
        assert_eq!(panel.selected.as_deref(), Some("$1"));
        assert!(!panel.shift && !panel.ctrl && !panel.alt);
        panel.active = true;
        panel.shift = true;
        panel.set_active(false);
        assert!(!panel.shift);
    }
    #[test]
    fn control_rows_align_without_reordering_keys() {
        for width in [360., 800.] {
            for alignment in [
                ControlAlignment::Left,
                ControlAlignment::Center,
                ControlAlignment::Right,
            ] {
                let context = egui::Context::default();
                crate::theme::apply(&context, crate::theme::Theme::TokyoNight.palette());
                let mut output = context.run_ui(
                    egui::RawInput {
                        screen_rect: Some(Rect::from_min_size(Pos2::ZERO, Vec2::new(width, 200.))),
                        ..Default::default()
                    },
                    |ctx| {
                        egui::CentralPanel::default().show(ctx, |ui| {
                            ui.spacing_mut().button_padding = Vec2::new(4., 6.);
                            ui.spacing_mut().interact_size = Vec2::splat(44.);
                            ui.spacing_mut().item_spacing = Vec2::splat(4.);
                            let available = ui.available_rect_before_wrap();
                            let mut bounds = Vec::new();
                            aligned_control_row(
                                ui,
                                alignment,
                                &["Ctrl", "Alt", "Shift", "Fn"],
                                2,
                                |ui| {
                                    for label in ["", "Ctrl", "Alt", "Shift", "Fn", ""] {
                                        bounds.push(
                                            ui.add_sized([44., 44.], egui::Button::new(label)).rect,
                                        );
                                    }
                                },
                            );
                            assert!(bounds.windows(2).all(|r| r[0].right() <= r[1].left()));
                            let row = bounds[0].union(bounds[5]);
                            let delta = match alignment {
                                ControlAlignment::Left => row.left() - available.left(),
                                ControlAlignment::Center => row.center().x - available.center().x,
                                ControlAlignment::Right => row.right() - available.right(),
                            };
                            assert!(delta.abs() < 1., "{alignment:?} at {width}: offset {delta}");
                        });
                    },
                );
                output.textures_delta.clear();
            }
        }
    }
    #[test]
    fn portrait_grid_has_native_ime_and_resizes_for_keyboard() {
        let context = egui::Context::default();
        install_font(&context);
        crate::theme::apply(&context, crate::theme::Theme::TokyoNight.palette());
        let mut panel = TerminalPanel::default();
        panel.attached = true;
        panel.typing = true;
        panel.status = "Connected".into();
        panel.sessions = vec![Session {
            id: "$0".into(),
            name: "a very long terminal session name repeated to exercise phone truncation".into(),
            working_directory: "/tmp".into(),
        }];
        panel.selected = Some("$0".into());
        let mut rows = 0;
        for height in [720., 420.] {
            let mut output = context.run_ui(
                egui::RawInput {
                    screen_rect: Some(Rect::from_min_size(Pos2::ZERO, Vec2::new(360., height))),
                    ..Default::default()
                },
                |context| {
                    egui::CentralPanel::default().show(context, |ui| {
                        panel.ui(ui);
                        assert!(
                            ui.min_rect().bottom() <= ui.max_rect().bottom() + 1.,
                            "both control rows must fit above the phone keyboard"
                        );
                        assert!(
                            ui.min_rect().right() <= ui.max_rect().right() + 1.,
                            "terminal controls must fit a narrow portrait viewport"
                        );
                    });
                },
            );
            assert!(output.platform_output.ime.is_some());
            assert!(!output.shapes.is_empty());
            output.textures_delta.clear();
            if rows > 0 {
                assert!(
                    !output
                        .platform_output
                        .ime
                        .unwrap()
                        .should_interrupt_composition,
                    "an idle focused terminal must not restart Android composition"
                );
                assert!(panel.size.rows < rows);
            }
            rows = panel.size.rows;
            assert!(panel.size.cols > 20);
        }
    }
}
