use std::sync::mpsc::{self, Receiver};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};
use std::thread;
use std::time::{Duration, Instant};

/// How long the fullscreen controls overlay stays visible before hiding.
const TOOLBAR_AUTO_HIDE: Duration = Duration::from_secs(4);

use eframe::egui::{self, Color32, ColorImage, RichText, TextureHandle, TextureOptions};
use ironrdp_client::config::{ConfigBuilder, Destination};
use ironrdp_client::output_channel::output_channel;
use ironrdp_client::rdp::{RdpClient, RdpInputEvent, RdpInputSender, RdpOutputEvent};
use ironrdp_pdu::input::fast_path::FastPathInputEvent;
use ironrdp_pdu::rdp::capability_sets::MajorPlatformType;
use smallvec::SmallVec;
use windowlink_shared::{DesktopSize, NormalizedPoint, Viewport};

mod clipboard;
mod connection_ui;
mod cursor;
mod desktop_targets;
mod file_browser;
mod hardware_keyboard;
mod icons;
mod keyboard;
mod net;
mod remote_input;
mod storage;
mod terminal;
mod theme;
mod window_picker;

#[cfg(test)]
mod display_tests;

#[derive(Clone, Copy, PartialEq, Eq)]
enum ViewMode {
    Overview,
    FullScreen,
    Files,
    Terminal,
}

fn theme_from_name(name: &str) -> theme::Theme {
    match name {
        "Catppuccin" => theme::Theme::Catppuccin,
        "Gruvbox" => theme::Theme::Gruvbox,
        "Rosé Pine" => theme::Theme::RosePine,
        _ => theme::Theme::TokyoNight,
    }
}

fn settings_menu_max_height(viewport_height: f32) -> f32 {
    (viewport_height - 32.0).max(1.0)
}

fn parse_rdp_destination(host: &str, port: u16) -> Result<Destination, String> {
    let bare_ipv6 = host
        .strip_prefix('[')
        .and_then(|value| value.strip_suffix(']'))
        .unwrap_or(host);
    let formatted_host = if bare_ipv6.parse::<std::net::Ipv6Addr>().is_ok() {
        format!("[{bare_ipv6}]")
    } else {
        host.to_owned()
    };
    Destination::new(format!("{formatted_host}:{port}")).map_err(|error| error.to_string())
}

/// Retry schedule after an unexpected session end. The first retry is quick so
/// a host restart reconnects almost immediately; later attempts are spaced out
/// and capped, then the app waits for the user instead of retrying forever.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ReconnectBackoff {
    attempts: u8,
}

impl ReconnectBackoff {
    /// Automatic retries before giving up and waiting for the user.
    const MAX_ATTEMPTS: u8 = 5;
    /// Wait before the first retry.
    const FIRST_DELAY: Duration = Duration::from_secs(3);
    /// Wait before every later retry.
    const RETRY_DELAY: Duration = Duration::from_secs(10);

    const fn new() -> Self {
        Self { attempts: 0 }
    }

    /// Record one retry and return how long to wait before it, or `None` once
    /// the attempt cap is reached.
    fn schedule(&mut self) -> Option<Duration> {
        if self.attempts >= Self::MAX_ATTEMPTS {
            return None;
        }
        let delay = if self.attempts == 0 {
            Self::FIRST_DELAY
        } else {
            Self::RETRY_DELAY
        };
        self.attempts += 1;
        Some(delay)
    }

    fn reset(&mut self) {
        self.attempts = 0;
    }
}

enum ClientEvent {
    Cursor(cursor::CursorUpdate),
    Status(String),
    Ended(String),
}

/// One complete decoded frame; replacing it never drops compressed H.264 data.
#[derive(Default)]
struct LatestFrame(Mutex<Option<(Vec<u8>, [usize; 2])>>);

impl LatestFrame {
    fn replace(&self, pixels: Vec<u8>, size: [usize; 2]) {
        let expected = size[0].checked_mul(size[1]).and_then(|n| n.checked_mul(4));
        if size.contains(&0) || expected != Some(pixels.len()) {
            log::warn!(
                "Ignoring malformed desktop frame: {:?}, {} bytes",
                size,
                pixels.len()
            );
            return;
        }
        *self.0.lock().unwrap_or_else(|error| error.into_inner()) = Some((pixels, size));
    }

    fn take(&self) -> Option<(Vec<u8>, [usize; 2])> {
        self.0
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .take()
    }
}

pub struct WindowLinkApp {
    host: String,
    port: String,
    control_host: String,
    control_port: String,
    username: String,
    password: String,
    status: String,
    started: bool,
    events: Option<Receiver<ClientEvent>>,
    latest_frame: Arc<LatestFrame>,
    stream_suppressed: bool,
    snapshot_requested_at: Option<Instant>,
    last_snapshot: Option<Instant>,
    input: Option<RdpInputSender>,
    texture: Option<TextureHandle>,
    desktop: Option<DesktopSize>,
    viewport: Viewport,
    remote_input: remote_input::RemoteInput,
    remote_cursor: cursor::RemoteCursor,
    cursor_tracker: Option<cursor::CursorTracker>,
    desktop_targets: desktop_targets::DesktopTargets,
    target_views: std::collections::HashMap<String, (Viewport, bool)>,
    keyboard: keyboard::KeyboardControls,
    terminal: terminal::TerminalPanel,
    file_browser: file_browser::FileBrowser,
    fit_screen: bool,
    /// Fullscreen live view: controls overlay visibility + auto-hide deadline.
    toolbar_visible: bool,
    toolbar_hide_at: Option<Instant>,
    clipboard_events_tx: mpsc::Sender<ClipboardEvent>,
    clipboard_events_rx: Receiver<ClipboardEvent>,
    file_picker_open: bool,
    reconnect_pending: bool,
    auto_reconnect: bool,
    reconnect_backoff: ReconnectBackoff,
    reconnect_deadline: Option<Instant>,
    clipboard_stop: Option<Arc<AtomicBool>>,
    clipboard_sync_state: Arc<Mutex<windowlink_shared::ClipboardSyncState>>,
    clipboard_sync_identity: Option<windowlink_shared::ClipboardSyncIdentity>,
    clipboard_sync_status: String,
    window_picker: window_picker::WindowPicker,
    theme: theme::Theme,
    applied_theme: Option<theme::Theme>,
    view_mode: ViewMode,
    show_keyboard: bool,
    connections_file: Option<std::path::PathBuf>,
    saved_connections: Vec<storage::SavedConnection>,
    preferences: storage::AppSettings,
    remember_connection: bool,
    connection_name: String,
    editing_connection: bool,
    editing_original: Option<(String, String)>,
    preferred_view: String,
    #[cfg(target_os = "android")]
    android_clipboard: Option<clipboard::AndroidClipboard>,
}

enum ClipboardEvent {
    Copied(Result<String, String>),
    Pasted(Result<(), String>),
    SyncStatus(String),
}

#[cfg(target_os = "android")]
#[unsafe(no_mangle)]
pub fn android_main(app: android_activity::AndroidApp) {
    #[cfg(target_os = "android")]
    {
        use android_logger::Config;
        android_logger::init_once(
            Config::default()
                .with_max_level(log::LevelFilter::Warn)
                .with_tag("OmaViewer"),
        );
    }
    let android_clipboard = clipboard::AndroidClipboard::new(app.clone());
    let connections_file = app.internal_data_path();
    let options = eframe::NativeOptions {
        android_app: Some(app),
        ..Default::default()
    };
    let result = eframe::run_native(
        "OmaViewer",
        options,
        Box::new(move |cc| {
            terminal::install_font(&cc.egui_ctx);
            let mut app_state = WindowLinkApp::default();
            app_state
                .file_browser
                .set_android(android_clipboard.clone());
            app_state.android_clipboard = Some(android_clipboard);
            app_state.connections_file = connections_file;
            app_state.saved_connections = storage::load(&app_state.connections_file);
            app_state.preferences = storage::load_preferences(&app_state.connections_file);
            app_state.apply_preferences_to_widgets();
            app_state
                .window_picker
                .set_preview_interval_secs(storage::load_preview_interval(
                    &app_state.connections_file,
                ));
            // Launch auto-connect: with exactly one saved connection, start the
            // session immediately via the same path as tapping it and pressing
            // Connect. Never let this crash startup. A later session end is
            // treated as unexpected and retried with backoff (`arm_reconnect`),
            // exactly like a session the user started by hand.
            if app_state.saved_connections.len() == 1 {
                let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    app_state.apply_saved(0);
                    app_state.start_connection();
                }));
                if result.is_err() {
                    log::error!("launch auto-connect panicked; continuing to form UI");
                    let mut recovered = WindowLinkApp::default();
                    recovered.android_clipboard = app_state.android_clipboard.take();
                    recovered.connections_file = app_state.connections_file.take();
                    recovered.saved_connections = std::mem::take(&mut app_state.saved_connections);
                    app_state = recovered;
                }
            }
            Ok(Box::new(app_state))
        }),
    );
    match result {
        Ok(()) => log::warn!("eframe::run_native returned normally"),
        Err(error) => log::error!("eframe::run_native failed: {error:?}"),
    }
}

impl Default for WindowLinkApp {
    fn default() -> Self {
        let (clipboard_events_tx, clipboard_events_rx) = mpsc::channel();
        Self {
            host: String::new(),
            port: "3389".into(),
            control_host: String::new(),
            control_port: "3391".into(),
            username: "windowlink".into(),
            password: String::new(),
            status: "Enter the host details printed by `windowlink-host serve`.".into(),
            started: false,
            events: None,
            latest_frame: Arc::new(LatestFrame::default()),
            stream_suppressed: false,
            snapshot_requested_at: None,
            last_snapshot: None,
            input: None,
            texture: None,
            desktop: None,
            viewport: Viewport::new(),
            remote_input: remote_input::RemoteInput::default(),
            remote_cursor: cursor::RemoteCursor::default(),
            cursor_tracker: None,
            desktop_targets: desktop_targets::DesktopTargets::default(),
            target_views: std::collections::HashMap::new(),
            keyboard: keyboard::KeyboardControls::default(),
            terminal: terminal::TerminalPanel::default(),
            file_browser: file_browser::FileBrowser::default(),
            fit_screen: true,
            toolbar_visible: true,
            toolbar_hide_at: None,
            clipboard_events_tx,
            clipboard_events_rx,
            file_picker_open: false,
            reconnect_pending: false,
            auto_reconnect: true,
            reconnect_backoff: ReconnectBackoff::new(),
            reconnect_deadline: None,
            clipboard_stop: None,
            clipboard_sync_state: Arc::new(
                Mutex::new(windowlink_shared::ClipboardSyncState::new()),
            ),
            clipboard_sync_identity: None,
            clipboard_sync_status: "Clipboard sync starting".into(),
            window_picker: window_picker::WindowPicker::default(),
            theme: theme::Theme::TokyoNight,
            applied_theme: None,
            view_mode: ViewMode::Overview,
            show_keyboard: false,
            connections_file: None,
            saved_connections: Vec::new(),
            preferences: storage::AppSettings::default(),
            remember_connection: true,
            connection_name: String::new(),
            editing_connection: false,
            editing_original: None,
            preferred_view: String::new(),
            #[cfg(target_os = "android")]
            android_clipboard: None,
        }
    }
}

impl WindowLinkApp {
    fn apply_preferences_to_widgets(&mut self) {
        self.terminal
            .set_font_size(f32::from(self.preferences.terminal_font_size));
        self.terminal.control_alignment = self.preferences.control_alignment;
        self.window_picker
            .set_preview_interval_secs(self.preferences.preview_interval_secs);
        self.file_browser
            .set_cache_limit_mib(self.preferences.staging_cache_max_mib);
        self.theme = theme_from_name(&self.preferences.theme);
        if let Ok(mut state) = self.clipboard_sync_state.lock() {
            state.set_enabled(
                self.preferences.text_clipboard_sync,
                self.preferences.image_clipboard_sync,
            );
        }
    }

    fn update_preferences(&mut self, preferences: storage::AppSettings) {
        if preferences == self.preferences {
            return;
        }
        self.preferences = preferences;
        self.apply_preferences_to_widgets();
        if let Err(error) = storage::store_preferences(&self.connections_file, &self.preferences) {
            self.status = format!("Could not save settings: {error}");
        }
    }

    fn remember_current(&mut self) {
        if !self.remember_connection || self.host.trim().is_empty() {
            return;
        }
        let conn = storage::SavedConnection {
            name: if self.connection_name.trim().is_empty() {
                self.host.trim().to_owned()
            } else {
                self.connection_name.trim().to_owned()
            },
            host: self.host.trim().to_owned(),
            password: self.password.clone(),
            username: self.username.clone(),
            port: self.port.trim().to_owned(),
            control_host: self.control_host.clone(),
            control_port: self.control_port.clone(),
            last_view: self.preferred_view.clone(),
        };
        if let Some((host, port)) = self.editing_original.take() {
            self.saved_connections
                .retain(|entry| !entry.matches(&host, &port));
        }
        storage::upsert(&mut self.saved_connections, conn);
        if let Err(error) = storage::store(&self.connections_file, &self.saved_connections) {
            self.status = format!("Could not save connection: {error}");
        }
    }

    fn persist_current_view(&mut self) {
        if !self.started {
            return;
        }
        let view = match self.view_mode {
            ViewMode::Terminal => "terminal",
            ViewMode::Files => "files",
            _ => "desktop",
        };
        if let Some(saved) = self
            .saved_connections
            .iter_mut()
            .find(|saved| saved.matches(&self.host, &self.port))
        {
            if saved.last_view != view {
                saved.last_view = view.into();
                self.preferred_view = view.into();
                let _ = storage::store(&self.connections_file, &self.saved_connections);
            }
        }
    }

    fn apply_saved(&mut self, index: usize) {
        let Some(conn) = self.saved_connections.get(index).cloned() else {
            return;
        };
        self.preferred_view = conn.last_view.clone();
        self.connection_name = conn.name.clone();
        self.host = conn.host.clone();
        self.password = conn.password.clone();
        self.username = if conn.username.is_empty() {
            "windowlink".into()
        } else {
            conn.username.clone()
        };
        self.port = if conn.port.is_empty() {
            "3389".into()
        } else {
            conn.port.clone()
        };
        self.control_host = conn.control_host.clone();
        self.control_port = if conn.control_port.is_empty() {
            "3391".into()
        } else {
            conn.control_port.clone()
        };
        self.remember_connection = true;
        self.status = format!("Loaded saved connection {host}", host = conn.host);
    }

    fn copy_desktop_clipboard(&mut self) {
        let host = self.api_host();
        let token = self.password.clone();
        let port = self.control_port.parse::<u16>().unwrap_or(3391);
        let tx = self.clipboard_events_tx.clone();
        self.status = "Copying desktop clipboard…".into();
        thread::spawn(move || {
            let result = clipboard::read_remote(&host, port, &token);
            let _ = tx.send(ClipboardEvent::Copied(result));
        });
    }

    fn paste_desktop_clipboard(&mut self) {
        #[cfg(target_os = "android")]
        let text = match self
            .android_clipboard
            .as_ref()
            .map(|clipboard| clipboard.get_text())
        {
            Some(Ok(text)) => text,
            Some(Err(error)) => {
                self.status = error;
                return;
            }
            None => {
                self.status = "Android clipboard is unavailable".into();
                return;
            }
        };
        #[cfg(not(target_os = "android"))]
        let text = String::new();

        let host = self.api_host();
        let token = self.password.clone();
        let port = self.control_port.parse::<u16>().unwrap_or(3391);
        let tx = self.clipboard_events_tx.clone();
        self.status = "Pasting into desktop clipboard…".into();
        thread::spawn(move || {
            let result = clipboard::write_remote(&host, port, &token, &text);
            let _ = tx.send(ClipboardEvent::Pasted(result));
        });
    }

    fn api_host(&self) -> String {
        let control_host = self.control_host.trim();
        if control_host.is_empty() {
            self.host.trim().to_owned()
        } else {
            control_host.to_owned()
        }
    }

    fn connect(&mut self) {
        self.auto_reconnect = true;
        let host = self.host.trim().to_owned();
        let username = self.username.clone();
        let password = self.password.clone();
        let port = self.port.parse::<u16>().unwrap_or(3389);
        if host.is_empty() {
            self.status = "Enter a host name or IP address.".into();
            return;
        }
        let destination = match parse_rdp_destination(&host, port) {
            Ok(destination) => destination,
            Err(error) => {
                self.status = format!("Invalid address: {error}");
                return;
            }
        };
        let config = ConfigBuilder::new()
            .with_destination(destination)
            .with_username(username.clone())
            .with_password(password.clone())
            .with_client_build(2600)
            .with_client_dir("C:\\")
            .with_client_name("OmaViewer Android")
            .with_platform(MajorPlatformType::WINDOWS)
            .with_desktop_width(1280)
            .with_desktop_height(720)
            .build();
        let config = match config {
            Ok(config) => config,
            Err(error) => {
                self.status = format!("Could not configure RDP: {error}");
                return;
            }
        };

        let identity = windowlink_shared::ClipboardSyncIdentity::new(
            host.clone(),
            port,
            self.api_host(),
            self.control_port.parse().unwrap_or(3391),
            username.clone(),
            password.clone(),
        );
        if self.clipboard_sync_identity.as_ref() != Some(&identity) {
            self.clipboard_sync_state =
                Arc::new(Mutex::new(windowlink_shared::ClipboardSyncState::new()));
            if let Ok(mut state) = self.clipboard_sync_state.lock() {
                state.set_enabled(
                    self.preferences.text_clipboard_sync,
                    self.preferences.image_clipboard_sync,
                );
            }
            self.clipboard_sync_identity = Some(identity);
        }

        let (tx, rx) = mpsc::channel();
        self.latest_frame = Arc::new(LatestFrame::default());
        self.remote_cursor = cursor::RemoteCursor::default();
        self.cursor_tracker = Some(cursor::CursorTracker::start(
            self.api_host(),
            self.control_port.parse().unwrap_or(3391),
            self.password.clone(),
        ));
        let latest_frame = self.latest_frame.clone();
        self.stream_suppressed = false;
        self.snapshot_requested_at = None;
        self.last_snapshot = None;
        let (output_sender, mut output_receiver) = output_channel(4);
        let client = RdpClient::new(config, output_sender);
        self.input = Some(client.input_sender());
        self.events = Some(rx);
        self.status = format!("Connecting to {host}:{port}…");
        self.started = true;
        #[cfg(target_os = "android")]
        self.start_clipboard_sync();
        thread::spawn(move || {
            let runtime = match tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
            {
                Ok(runtime) => runtime,
                Err(error) => {
                    let _ = tx.send(ClientEvent::Ended(format!("Runtime error: {error}")));
                    return;
                }
            };
            runtime.block_on(async move {
                let forward_tx = tx.clone();
                let forward_task = tokio::spawn(async move {
                    let mut connected_announced = false;
                    while let Some(event) = output_receiver.recv().await {
                        match event {
                            RdpOutputEvent::Image {
                                buffer,
                                width,
                                height,
                            } => {
                                if !connected_announced {
                                    let _ =
                                        forward_tx.send(ClientEvent::Status("Connected".into()));
                                    connected_announced = true;
                                }
                                // `buffer` is RGBA8 bytes; pass through untouched so the
                                // UI thread can cast it into `Color32` without per-pixel work.
                                latest_frame
                                    .replace(buffer, [width.get() as usize, height.get() as usize]);
                            }
                            RdpOutputEvent::PointerDefault => {
                                let _ = forward_tx
                                    .send(ClientEvent::Cursor(cursor::CursorUpdate::Default));
                            }
                            RdpOutputEvent::PointerHidden => {
                                let _ = forward_tx
                                    .send(ClientEvent::Cursor(cursor::CursorUpdate::Hidden));
                            }
                            RdpOutputEvent::PointerPosition { x, y } => {
                                let _ = forward_tx.send(ClientEvent::Cursor(
                                    cursor::CursorUpdate::Position([x, y]),
                                ));
                            }
                            RdpOutputEvent::PointerBitmap(pointer) => {
                                let _ = forward_tx.send(ClientEvent::Cursor(
                                    cursor::CursorUpdate::Bitmap {
                                        size: [pointer.width as usize, pointer.height as usize],
                                        hotspot: [pointer.hotspot_x, pointer.hotspot_y],
                                        rgba: pointer.bitmap_data.clone(),
                                    },
                                ));
                            }
                            RdpOutputEvent::ConnectionFailure(error) => {
                                let _ = forward_tx.send(ClientEvent::Ended(format!(
                                    "Connection failed: {error}"
                                )));
                            }
                            RdpOutputEvent::Terminated(result) => {
                                let reason = match result {
                                    Ok(reason) => format!("{reason:?}"),
                                    Err(error) => format!("{error:?}"),
                                };
                                log::warn!("session terminated: {reason}");
                                let _ = forward_tx
                                    .send(ClientEvent::Ended(format!("Session ended: {reason}")));
                            }
                            _ => {}
                        }
                    }
                });
                client.run().await;
                log::warn!("RDP client run loop exited");
                forward_task.abort();
                let _ = forward_task.await;
                let _ = tx.send(ClientEvent::Ended("Session ended".into()));
            });
        });
    }

    /// Begin a session from the current form values. The Connect button and the
    /// saved-connection chips both go through here so a chip connects in one tap.
    fn start_connection(&mut self) {
        self.window_picker.reset();
        self.desktop_targets = desktop_targets::DesktopTargets::default();
        self.target_views.clear();
        self.view_mode = match self.preferred_view.as_str() {
            "files" => ViewMode::Files,
            "terminal" => ViewMode::Terminal,
            _ => ViewMode::Overview,
        };
        self.reconnect_backoff.reset();
        self.reconnect_deadline = None;
        self.reconnect_pending = false;
        self.connect();
        if self.started {
            self.window_picker.refresh(
                &self.api_host(),
                self.control_port.parse().unwrap_or(3391),
                &self.password,
            );
            self.remember_current();
        }
    }

    /// Schedule the next automatic retry after an unexpected session end, or
    /// give up once the attempt cap is reached.
    fn arm_reconnect(&mut self, reason: &str) {
        match self.reconnect_backoff.schedule() {
            Some(delay) => {
                self.reconnect_pending = true;
                self.reconnect_deadline = Some(Instant::now() + delay);
                self.status = format!("{reason}. Retrying in {}s", delay.as_secs());
            }
            None => {
                self.auto_reconnect = false;
                self.status = format!("{reason}. Automatic retries stopped");
            }
        }
    }

    /// Stop automatic retries. Called on a user-initiated disconnect and when
    /// the address field is edited, so the app only retries the connection the
    /// user actually asked for.
    fn cancel_reconnect(&mut self) {
        self.auto_reconnect = false;
        self.reconnect_pending = false;
        self.reconnect_deadline = None;
        self.reconnect_backoff.reset();
    }

    /// Enter the fullscreen live view: rotate to landscape and show the
    /// controls overlay until the auto-hide timer expires.
    fn enter_fullscreen(&mut self) {
        self.view_mode = ViewMode::FullScreen;
        self.toolbar_visible = true;
        self.toolbar_hide_at = Some(Instant::now() + TOOLBAR_AUTO_HIDE);
    }

    fn disconnect(&mut self) {
        self.terminal.set_active(false);
        self.file_browser.disconnect();
        self.preferred_view = match self.view_mode {
            ViewMode::Files => "files",
            ViewMode::Terminal => "terminal",
            _ => "desktop",
        }
        .into();
        if let Some(saved) = self
            .saved_connections
            .iter_mut()
            .find(|s| s.matches(&self.host, &self.port))
        {
            saved.last_view = self.preferred_view.clone();
            let _ = storage::store(&self.connections_file, &self.saved_connections);
        }

        hardware_keyboard::set_remote_focus(false);
        self.cursor_tracker = None;
        let releases = self.remote_input.release_all();
        self.send_operations(releases);
        let key_releases = self.keyboard.reset();
        self.send_operations(key_releases);
        self.file_picker_open = false;
        self.cancel_reconnect();
        if let Some(stop) = self.clipboard_stop.take() {
            stop.store(true, Ordering::Relaxed);
        }
        self.close_capture();
        self.started = false;
        self.status = "Disconnected".into();
    }

    fn close_capture(&mut self) {
        let releases = self.remote_input.release_all();
        self.send_operations(releases);
        let keys = self.keyboard.reset();
        self.send_operations(keys);
        hardware_keyboard::set_remote_focus(false);
        self.cursor_tracker = None;
        if let Some(input) = &self.input {
            let _ = input.try_send(RdpInputEvent::Close);
        }
        self.events = None;
        self.latest_frame.take();
        self.input = None;
        self.texture = None;
        self.desktop = None;
    }

    #[cfg(target_os = "android")]
    fn start_clipboard_sync(&mut self) {
        let Some(android) = self.android_clipboard.clone() else {
            return;
        };
        if let Some(old) = self.clipboard_stop.take() {
            old.store(true, Ordering::Relaxed);
        }
        let stop = Arc::new(AtomicBool::new(false));
        self.clipboard_stop = Some(stop.clone());
        self.clipboard_sync_status = "Clipboard sync starting".into();
        let host = self.api_host();
        let token = self.password.clone();
        let port = self.control_port.parse().unwrap_or(3391);
        let tx = self.clipboard_events_tx.clone();
        let sync_state = self.clipboard_sync_state.clone();
        thread::spawn(move || {
            clipboard::run_typed_sync(android, host, port, token, stop, sync_state, |status| {
                let _ = tx.send(ClipboardEvent::SyncStatus(status));
            })
        });
    }

    fn send_operations(&mut self, events: impl IntoIterator<Item = FastPathInputEvent>) {
        let Some(sender) = &self.input else { return };
        // Fast-path packets accept at most 255 events. Bound text input batches
        // while keeping press/release pairs together and in order.
        let mut events = events.into_iter().peekable();
        while events.peek().is_some() {
            let batch: SmallVec<[FastPathInputEvent; 2]> = events.by_ref().take(128).collect();
            if sender.try_send(RdpInputEvent::FastPath(batch)).is_err() {
                break;
            }
        }
    }

    pub(crate) fn settings_menu(&mut self, ui: &mut egui::Ui) {
        let mut settings = self.preferences.clone();
        ui.menu_button("Settings", |ui| {
            let max_height = settings_menu_max_height(ui.ctx().content_rect().height());
            egui::ScrollArea::vertical()
                .max_height(max_height)
                .show(ui, |ui| {
                    ui.set_min_width(260.0);
                    ui.label("Appearance");
                    egui::ComboBox::from_id_salt("settings-theme")
                        .selected_text(&settings.theme)
                        .show_ui(ui, |ui| {
                            for theme in theme::Theme::ALL {
                                ui.selectable_value(
                                    &mut settings.theme,
                                    theme.name().to_owned(),
                                    theme.name(),
                                );
                            }
                        });
                    ui.separator();
                    ui.label("Terminal");
                    let mut font_size = f32::from(settings.terminal_font_size);
                    ui.add(egui::Slider::new(&mut font_size, 10.0..=28.0).text("Font size"));
                    settings.terminal_font_size = font_size.round().clamp(10.0, 28.0) as u8;
                    egui::ComboBox::from_id_salt("settings-control-alignment")
                        .selected_text(settings.control_alignment.label())
                        .show_ui(ui, |ui| {
                            for alignment in [
                                storage::ControlAlignment::Left,
                                storage::ControlAlignment::Center,
                                storage::ControlAlignment::Right,
                            ] {
                                ui.selectable_value(
                                    &mut settings.control_alignment,
                                    alignment,
                                    alignment.label(),
                                );
                            }
                        });
                    ui.separator();
                    ui.label("Previews");
                    egui::ComboBox::from_id_salt("settings-preview-interval")
                        .selected_text(if settings.preview_interval_secs == 0 {
                            "Manual".into()
                        } else {
                            format!("Every {} seconds", settings.preview_interval_secs)
                        })
                        .show_ui(ui, |ui| {
                            for seconds in [5, 10, 30, 60, 300, 0] {
                                let label = if seconds == 0 {
                                    "Manual".into()
                                } else {
                                    format!("Every {seconds} seconds")
                                };
                                ui.selectable_value(
                                    &mut settings.preview_interval_secs,
                                    seconds,
                                    label,
                                );
                            }
                        });
                    ui.separator();
                    ui.label("Clipboard sync");
                    ui.checkbox(&mut settings.text_clipboard_sync, "Text");
                    ui.checkbox(&mut settings.image_clipboard_sync, "Images");
                    ui.separator();
                    ui.label("File transfer cache");
                    ui.add(
                        egui::Slider::new(&mut settings.staging_cache_max_mib, 64..=4096)
                            .step_by(64.0)
                            .text("MiB"),
                    );
                });
        });
        self.update_preferences(settings);
    }

    fn connection_field(
        ui: &mut egui::Ui,
        palette: theme::Palette,
        label: &str,
        value: &mut String,
        hint: &str,
        password: bool,
    ) {
        ui.label(
            RichText::new(label)
                .size(16.0)
                .strong()
                .color(palette.muted),
        );
        let width = ui.available_width();
        egui::Frame::new()
            .fill(palette.surface)
            .stroke(egui::Stroke::new(1.0, palette.border))
            .corner_radius(12.0)
            .inner_margin(egui::Margin::symmetric(14, 4))
            .show(ui, |ui| {
                let editor = egui::TextEdit::singleline(value)
                    .hint_text(hint)
                    .password(password)
                    .frame(egui::Frame::NONE)
                    .margin(0.0)
                    .desired_width((width - 28.0).max(60.0));
                ui.add_sized([(width - 28.0).max(60.0), 48.0], editor);
            });
    }

    fn draw_header(&mut self, ui: &mut egui::Ui, palette: theme::Palette) {
        if self.show_keyboard
            && self.view_mode == ViewMode::FullScreen
            && ui.ctx().content_rect().width() > ui.ctx().content_rect().height()
        {
            return;
        }
        if self.view_mode == ViewMode::FullScreen {
            egui::Frame::new()
                .inner_margin(egui::Margin::symmetric(18, 8))
                .show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    ui.horizontal(|ui| {
                        if ui
                            .add_sized(
                                [128.0, 48.0],
                                egui::Button::new("< Windows").fill(palette.surface),
                            )
                            .clicked()
                        {
                            let releases = self.remote_input.release_all();
                            self.send_operations(releases);
                            let key_releases = self.keyboard.reset();
                            self.send_operations(key_releases);
                            self.view_mode = ViewMode::Overview;
                            self.window_picker.refresh(
                                &self.api_host(),
                                self.control_port.parse().unwrap_or(3391),
                                &self.password,
                            );
                        }
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if self.show_keyboard {
                                if ui
                                    .add_sized([100.0, 48.0], egui::Button::new("Done"))
                                    .clicked()
                                {
                                    self.show_keyboard = false;
                                    let releases = self.keyboard.reset();
                                    self.send_operations(releases);
                                    self.toolbar_hide_at = Some(Instant::now() + TOOLBAR_AUTO_HIDE);
                                }
                                return;
                            }
                            ui.label(
                                RichText::new(if self.desktop.is_some() {
                                    "LIVE"
                                } else {
                                    "WAITING"
                                })
                                .size(14.0)
                                .strong()
                                .color(palette.green),
                            );
                        });
                    });
                });
            return;
        }
        egui::Frame::new()
            .inner_margin(egui::Margin::symmetric(18, 10))
            .show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.horizontal(|ui| {
                    ui.label(
                        RichText::new("OMAVIEWER")
                            .size(17.0)
                            .strong()
                            .color(palette.accent),
                    );
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        self.settings_menu(ui);
                    });
                });
            });
    }

    fn draw_navigation(&mut self, ui: &mut egui::Ui, palette: theme::Palette) {
        self.draw_desktop_switcher(ui, palette);
        ui.horizontal(|ui| {
            for (icon, label, view) in [
                (icons::Icon::Desktop, "Desktop", ViewMode::Overview),
                (icons::Icon::Terminal, "Terminal", ViewMode::Terminal),
                (icons::Icon::Folder, "Files", ViewMode::Files),
            ] {
                if icons::button(ui, icon, label, true, self.view_mode == view).clicked() {
                    self.view_mode = view;
                }
            }
        });
    }

    fn draw_preview_settings(&mut self, ui: &mut egui::Ui, palette: theme::Palette) {
        ui.collapsing(RichText::new("Preview settings").size(17.0), |ui| {
            let previous = self.window_picker.preview_interval_secs();
            let mut interval = previous;
            ui.label("Refresh unopened windows and the desktop snapshot");
            egui::ComboBox::from_id_salt("preview-interval")
                .selected_text(if interval == 0 {
                    "Manual".into()
                } else {
                    format!("Every {interval} seconds")
                })
                .show_ui(ui, |ui| {
                    for seconds in [5, 10, 30, 60, 300, 0] {
                        let label = if seconds == 0 {
                            "Manual".into()
                        } else {
                            format!("Every {seconds} seconds")
                        };
                        ui.selectable_value(&mut interval, seconds, label);
                    }
                });
            if interval != previous {
                let mut settings = self.preferences.clone();
                settings.preview_interval_secs = interval;
                self.update_preferences(settings);
            }
            ui.label(
                RichText::new("Live video runs when you open a window or desktop.")
                    .size(14.0)
                    .color(palette.muted),
            );
            if ui.button("Refresh snapshots now").clicked() {
                self.last_snapshot = None;
                self.window_picker.refresh(
                    &self.api_host(),
                    self.control_port.parse().unwrap_or(3391),
                    &self.password,
                );
            }
        });
    }

    /// Suppress graphics on the server between overview snapshots. The RDP
    /// session and separate clipboard/file channels stay connected.
    fn update_stream_visibility(&mut self) {
        if !self.started || self.input.is_none() {
            return;
        }
        let now = Instant::now();
        let interval = self.window_picker.preview_interval_secs();
        let snapshot_due = self.view_mode == ViewMode::Overview
            && self
                .last_snapshot
                .is_none_or(|last| interval != 0 && now.duration_since(last).as_secs() >= interval);
        let want_live = self.view_mode == ViewMode::FullScreen;
        let waiting_for_snapshot = self
            .snapshot_requested_at
            .is_some_and(|start| now.duration_since(start) < Duration::from_secs(3));
        let enabled = want_live
            || (self.view_mode == ViewMode::Overview
                && ((snapshot_due && self.snapshot_requested_at.is_none())
                    || waiting_for_snapshot));
        if enabled && !want_live && self.snapshot_requested_at.is_none() && snapshot_due {
            self.snapshot_requested_at = Some(now);
        }
        if enabled == !self.stream_suppressed {
            return;
        }
        if self
            .input
            .as_ref()
            .unwrap()
            .try_send(RdpInputEvent::SetDisplayUpdates(enabled))
            .is_ok()
        {
            self.stream_suppressed = !enabled;
            log::info!(
                "Display updates {}",
                if enabled { "resumed" } else { "paused" }
            );
            if !enabled
                && self
                    .snapshot_requested_at
                    .is_some_and(|start| now.duration_since(start) >= Duration::from_secs(3))
            {
                // Back off after a snapshot timeout, without counting a Files
                // navigation that cancelled a request as a successful snapshot.
                self.last_snapshot = Some(now);
            }
            self.snapshot_requested_at = (enabled && !want_live).then_some(now);
        }
    }

    fn draw_overview(&mut self, ui: &mut egui::Ui, palette: theme::Palette) {
        egui::ScrollArea::vertical().show(ui, |ui| {
            egui::Frame::new()
                .inner_margin(egui::Margin::symmetric(18, 18))
                .show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    ui.add_space(4.0);
                    let outer_width = ui.available_width();
                    egui::Frame::new()
                        .fill(palette.surface)
                        .stroke(egui::Stroke::new(1.0, palette.border))
                        .corner_radius(18.0)
                        .inner_margin(12.0)
                        .show(ui, |ui| {
                            ui.set_min_width((outer_width - 24.0).max(0.0));
                            let width = ui.available_width();
                            let height = (width * 0.40).clamp(100.0, 200.0);
                            let (rect, response) = ui.allocate_exact_size(
                                egui::vec2(width, height),
                                egui::Sense::click(),
                            );
                            ui.painter().rect_filled(rect, 10.0, palette.background);
                            if let Some(texture) = &self.texture {
                                let size = texture.size_vec2();
                                let fit = (rect.width() / size.x).min(rect.height() / size.y);
                                let target =
                                    egui::Rect::from_center_size(rect.center(), size * fit);
                                ui.painter().image(
                                    texture.id(),
                                    target,
                                    egui::Rect::from_min_max(
                                        egui::Pos2::ZERO,
                                        egui::pos2(1.0, 1.0),
                                    ),
                                    Color32::WHITE,
                                );
                            } else {
                                ui.painter().text(
                                    rect.center(),
                                    egui::Align2::CENTER_CENTER,
                                    if !self.auto_reconnect {
                                        "Select a window below"
                                    } else {
                                        "Connecting to desktop…"
                                    },
                                    egui::FontId::proportional(17.0),
                                    palette.muted,
                                );
                            }
                            if response.clicked() {
                                self.enter_fullscreen();
                            }
                            ui.add_space(8.0);
                            if ui
                                .add_sized(
                                    [width, 50.0],
                                    egui::Button::new("Open live view").fill(palette.raised),
                                )
                                .clicked()
                            {
                                self.enter_fullscreen();
                            }
                        });
                    ui.add_space(14.0);
                    self.window_picker.ui(
                        ui,
                        palette,
                        &self.api_host(),
                        self.control_port.parse().unwrap_or(3391),
                        &self.password,
                    );
                    ui.add_space(16.0);
                    self.draw_preview_settings(ui, palette);
                });
        });
    }

    fn draw_toolbar(&mut self, ui: &mut egui::Ui, palette: theme::Palette) {
        use remote_input::InputMode;
        if self.show_keyboard {
            egui::Frame::new()
                .inner_margin(egui::Margin::symmetric(18, 4))
                .show(ui, |ui| {
                    let landscape =
                        ui.ctx().content_rect().width() > ui.ctx().content_rect().height();
                    if landscape {
                        ui.horizontal(|ui| {
                            if ui
                                .add_sized([72.0, 48.0], egui::Button::new("Done"))
                                .clicked()
                            {
                                self.show_keyboard = false;
                                let releases = self.keyboard.reset();
                                self.send_operations(releases);
                                self.toolbar_hide_at = Some(Instant::now() + TOOLBAR_AUTO_HIDE);
                            }
                            let events = self.keyboard.show(ui);
                            self.send_operations(events);
                        });
                    } else {
                        let events = self.keyboard.show(ui);
                        self.send_operations(events);
                    }
                });
            return;
        }
        egui::Frame::new()
            .inner_margin(egui::Margin::symmetric(18, 8))
            .show(ui, |ui| {
                ui.set_width(ui.available_width());
                let width =
                    ((ui.available_width() - ui.spacing().item_spacing.x * 3.0) / 4.0).max(48.0);
                ui.horizontal(|ui| {
                    for (label, mode) in [
                        ("Click", InputMode::Touch),
                        ("Scroll", InputMode::Scroll),
                        ("Pan", InputMode::Pan),
                        ("Trackpad", InputMode::Trackpad),
                    ] {
                        if ui
                            .add_sized(
                                [width, 48.0],
                                egui::Button::new(label).selected(self.remote_input.mode() == mode),
                            )
                            .clicked()
                        {
                            self.remote_input.set_mode(mode);
                        }
                    }
                });
                ui.horizontal(|ui| {
                    if ui
                        .add_sized(
                            [width, 48.0],
                            egui::Button::new("Keyboard").selected(self.show_keyboard),
                        )
                        .clicked()
                    {
                        self.show_keyboard = !self.show_keyboard;
                        if !self.show_keyboard {
                            let key_releases = self.keyboard.reset();
                            self.send_operations(key_releases);
                        }
                    }
                    let zoomed = self.viewport.zoom() > 1.001;
                    let fit_label = if self.fit_screen && !zoomed {
                        "Fill view"
                    } else {
                        "Fit whole"
                    };
                    if ui
                        .add_sized([width, 48.0], egui::Button::new(fit_label))
                        .clicked()
                    {
                        self.fit_screen = zoomed || !self.fit_screen;
                        self.viewport.reset();
                    }
                    ui.menu_button("More", |ui| {
                        if ui.button("Right-click next tap").clicked() {
                            self.remote_input.arm_right_click();
                            ui.close();
                        }
                        for (label, factor) in [("Zoom in", 1.5), ("Zoom out", 1.0 / 1.5)] {
                            if ui.button(label).clicked() {
                                self.viewport
                                    .zoom_at(factor, NormalizedPoint::new(0.5, 0.5));
                                ui.close();
                            }
                        }
                        if ui.button("Copy desktop text to phone").clicked() {
                            self.copy_desktop_clipboard();
                            ui.close();
                        }
                        if ui.button("Copy phone text to desktop").clicked() {
                            self.paste_desktop_clipboard();
                            ui.close();
                        }
                    });
                });
                if self.remote_input.mode() == InputMode::Trackpad {
                    let held = self.remote_input.drag_held();
                    if ui
                        .button(if held { "Release drag" } else { "Drag" })
                        .clicked()
                    {
                        let events = if held {
                            self.remote_input.release_drag()
                        } else {
                            self.remote_input.start_drag()
                        };
                        self.send_operations(events);
                    }
                }
                let hint = if self.remote_input.right_click_armed() {
                    "Tap a target in the window to right-click"
                } else {
                    match self.remote_input.mode() {
                        InputMode::Touch => "Tap then hold to right-click. Drag to move or select",
                        InputMode::Scroll => "Swipe the window to scroll its contents",
                        InputMode::Pan => "Drag to move the view. Pinch to zoom",
                        InputMode::Trackpad => "Slide to point. Tap then hold to right-click",
                    }
                };
                ui.label(RichText::new(hint).size(15.0).color(palette.muted));
            });
    }

    /// Draw the desktop frame inside `rect` and forward pointer input.
    fn draw_stream(
        &mut self,
        ctx: &egui::Context,
        ui: &mut egui::Ui,
        _palette: theme::Palette,
        rect: egui::Rect,
        response: egui::Response,
    ) {
        if let Some(texture) = &self.texture {
            let texture_size = texture.size_vec2();
            let desktop = self.desktop.unwrap_or(DesktopSize::new(
                texture_size.x as u32,
                texture_size.y as u32,
            ));
            let view_aspect = rect.width() as f64 / rect.height() as f64;
            let (image_rect, uv) = if self.fit_screen {
                let fit = (rect.width() / texture_size.x).min(rect.height() / texture_size.y);
                let image_rect = egui::Rect::from_center_size(rect.center(), texture_size * fit);
                let center = self.viewport.center();
                let half = 0.5 / self.viewport.zoom();
                (
                    image_rect,
                    egui::Rect::from_min_max(
                        egui::pos2((center.x - half) as f32, (center.y - half) as f32),
                        egui::pos2((center.x + half) as f32, (center.y + half) as f32),
                    ),
                )
            } else {
                let (min, max) = self.viewport.aspect_bounds(desktop, view_aspect);
                (
                    rect,
                    egui::Rect::from_min_max(
                        egui::pos2(min.x as f32, min.y as f32),
                        egui::pos2(max.x as f32, max.y as f32),
                    ),
                )
            };
            ui.painter()
                .image(texture.id(), image_rect, uv, Color32::WHITE);
            self.remote_input
                .set_blocked_rect((!self.toolbar_visible).then(|| {
                    egui::Rect::from_min_size(
                        egui::pos2(rect.right() - 44.0, rect.top() + 10.0),
                        egui::vec2(34.0, 34.0),
                    )
                }));
            let events = self.remote_input.handle(
                ctx,
                &response,
                image_rect,
                desktop,
                &mut self.viewport,
                self.fit_screen,
            );
            self.send_operations(events);
            if ctx.input(|i| i.pointer.any_down()) {
                self.remote_cursor.outside_target = false;
            }
            self.remote_cursor.paint(
                ui,
                image_rect,
                uv,
                [desktop.width, desktop.height],
                self.remote_input.cursor_position(),
            );
            if let Some((point, progress)) = self.remote_input.hold_progress(ctx.input(|i| i.time))
            {
                ui.painter().circle_stroke(
                    point,
                    22.0,
                    egui::Stroke::new(2.0 + 4.0 * progress, _palette.accent),
                );
            }
        } else {
            ui.painter().text(
                rect.center(),
                egui::Align2::CENTER_CENTER,
                "Waiting for remote desktop…",
                egui::FontId::proportional(20.0),
                Color32::WHITE,
            );
        }
    }

    /// Fullscreen live view with the controls hidden: the stream fills the
    /// screen and a small floating handle reopens the overlay.
    fn draw_fullscreen_stream(
        &mut self,
        ctx: &egui::Context,
        ui: &mut egui::Ui,
        palette: theme::Palette,
    ) {
        let (rect, response) =
            ui.allocate_exact_size(ui.available_size(), egui::Sense::click_and_drag());
        ui.painter().rect_filled(rect, 0.0, palette.background);
        self.draw_stream(ctx, ui, palette, rect, response);
        // Floating handle, drawn last so it sits on top of the stream.
        let handle_rect = egui::Rect::from_min_size(
            egui::pos2(rect.right() - 44.0, rect.top() + 10.0),
            egui::vec2(34.0, 34.0),
        );
        let handle = ui.allocate_rect(handle_rect, egui::Sense::click());
        let alpha = if handle.hovered() { 220 } else { 150 };
        ui.painter()
            .rect_filled(handle_rect, 10.0, Color32::from_black_alpha(alpha));
        ui.painter().text(
            handle_rect.center(),
            egui::Align2::CENTER_CENTER,
            "☰",
            egui::FontId::proportional(18.0),
            Color32::WHITE,
        );
        if handle.clicked() {
            self.toolbar_visible = true;
            self.toolbar_hide_at = Some(Instant::now() + TOOLBAR_AUTO_HIDE);
        }
    }
}

#[cfg(test)]
mod destination_tests {
    use super::parse_rdp_destination;

    #[test]
    fn rdp_destination_accepts_dns_and_both_ipv6_forms() {
        assert_eq!(
            parse_rdp_destination("desktop.example.net", 3390)
                .unwrap()
                .to_string(),
            "desktop.example.net:3390"
        );
        for host in ["2001:db8::1", "[2001:db8::1]"] {
            assert_eq!(
                parse_rdp_destination(host, 3389).unwrap().to_string(),
                "[2001:db8::1]:3389"
            );
        }
    }
}

impl eframe::App for WindowLinkApp {
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        if self.view_mode == ViewMode::Terminal {
            self.terminal.maintain_ime(ctx);
        }
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        self.file_picker_open = self.file_browser.picker_open();
        if self.view_mode != ViewMode::FullScreen
            || self.texture.is_none()
            || !ctx.input(|i| i.focused)
            || self.file_picker_open
        {
            let releases = self.remote_input.observe_inactive(&ctx);
            self.send_operations(releases);
        }

        let remote_focus = self.view_mode == ViewMode::FullScreen
            && self.desktop.is_some()
            && ctx.input(|i| i.focused)
            && (!ctx.egui_wants_keyboard_input() || self.keyboard.typing_has_focus(&ctx));
        hardware_keyboard::set_remote_focus(remote_focus);
        match hardware_keyboard::drain() {
            Ok(events) if remote_focus => {
                for event in events {
                    let keys = self.keyboard.handle_hardware_scan_code(
                        event.code,
                        event.extended,
                        event.pressed,
                    );
                    self.send_operations(keys);
                }
            }
            Err(error) => {
                self.disconnect();
                self.status = error.into();
            }
            _ => {}
        }
        let hardware = ctx.input_mut(|i| {
            self.keyboard
                .handle_hardware_events(&mut i.events, remote_focus)
        });
        self.send_operations(hardware);
        if self.started {
            self.terminal.configure(
                self.api_host(),
                self.control_port.parse().unwrap_or(3391),
                self.password.clone(),
            );
        }
        self.terminal.set_active(
            self.started && self.view_mode == ViewMode::Terminal && ctx.input(|i| i.focused),
        );
        if self.started {
            self.file_browser.set_connection(
                &self.api_host(),
                self.control_port.parse().unwrap_or(3391),
                &self.password,
            );
            if self.file_browser.poll(&ctx) {
                self.view_mode = ViewMode::Files;
            }
            while let Some(path) = self.file_browser.take_insert_path() {
                self.terminal.insert_path(&path);
                self.view_mode = ViewMode::Terminal;
            }
        }
        while let Ok(event) = self.clipboard_events_rx.try_recv() {
            match event {
                ClipboardEvent::Copied(Ok(text)) => {
                    #[cfg(target_os = "android")]
                    let result = self
                        .android_clipboard
                        .as_ref()
                        .ok_or_else(|| "Android clipboard is unavailable".to_owned())
                        .and_then(|clipboard| clipboard.set_text(&text));
                    #[cfg(not(target_os = "android"))]
                    let result: Result<(), String> = {
                        let _ = text;
                        Err("Android clipboard is unavailable".into())
                    };
                    self.status = match result {
                        Ok(()) => "Copied desktop text to Android clipboard.".into(),
                        Err(error) => error,
                    };
                }
                ClipboardEvent::Copied(Err(error)) => self.status = error,
                ClipboardEvent::Pasted(Ok(())) => {
                    self.status = "Pasted Android text to desktop clipboard.".into()
                }
                ClipboardEvent::Pasted(Err(error)) => self.status = error,
                ClipboardEvent::SyncStatus(status) => {
                    self.clipboard_sync_status = status.clone();
                    self.status = status;
                }
            }
        }
        // Hold the terminal state until the event-receiver borrow ends, so the
        // reconnect schedule can take `&mut self`.
        let mut session_ended: Option<String> = None;
        if let Some(events) = &self.events {
            // The decode thread can queue several frames between repaints; only the
            // newest one is worth uploading, older frames would just add latency.
            while let Ok(event) = events.try_recv() {
                match event {
                    ClientEvent::Cursor(update) => {
                        if let Some(point) = self.remote_cursor.update(&ctx, update) {
                            self.remote_input.set_cursor_position(point);
                        }
                    }
                    ClientEvent::Status(status) => self.status = status,
                    ClientEvent::Ended(status) => {
                        session_ended = Some(status);
                        self.input = None;
                        self.texture = None;
                        self.desktop = None;
                        self.remote_input = remote_input::RemoteInput::default();
                        // Keep the window picker and file controls usable when
                        // an isolated window closes or its stream reconnects.
                        if self.view_mode == ViewMode::FullScreen {
                            self.view_mode = ViewMode::Overview;
                        }
                        // The transport is already closed; the host releases its keys.
                        let _ = self.keyboard.reset();
                    }
                }
            }
            // A terminal event always wins over a frame from the same UI cycle.
            if session_ended.is_some() {
                self.latest_frame.take();
            } else if let Some((pixels, size)) = self.latest_frame.take() {
                self.reconnect_backoff.reset();
                self.reconnect_deadline = None;
                self.reconnect_pending = false;
                self.last_snapshot = Some(Instant::now());
                self.snapshot_requested_at = None;
                self.desktop = Some(DesktopSize::new(size[0] as u32, size[1] as u32));
                let image = ColorImage::new(size, cast_rgba8_to_color32(pixels));
                if let Some(texture) = &mut self.texture {
                    texture.set(image, TextureOptions::LINEAR);
                } else {
                    self.texture =
                        Some(ctx.load_texture("remote-desktop", image, TextureOptions::LINEAR));
                }
            }
        }
        if let Some(status) = session_ended {
            self.events = None;
            if self.auto_reconnect {
                // Keep the picker available while video retries with backoff.
                self.arm_reconnect(&status);
            } else {
                // User-initiated disconnect: leave the status alone.
                self.status = status;
            }
        }
        if self.reconnect_pending
            && !self.file_picker_open
            && self
                .reconnect_deadline
                .is_some_and(|deadline| Instant::now() >= deadline)
        {
            self.reconnect_pending = false;
            self.reconnect_deadline = None;
            self.status = "Reconnecting to desktop…".into();
            self.fit_screen = true;
            self.viewport.reset();
            self.connect();
        }
        if let Some(tracker) = &self.cursor_tracker {
            tracker.set_active(
                self.view_mode == ViewMode::FullScreen
                    && ctx.input(|i| i.focused)
                    && !self.file_picker_open,
            );
            if let Some(desktop) = self.desktop {
                if let Some((position, visible)) = tracker.poll([desktop.width, desktop.height]) {
                    if !ctx.input(|i| i.pointer.any_down()) && !self.remote_input.drag_held() {
                        self.remote_cursor.outside_target = !visible;
                        if visible {
                            self.remote_input.set_cursor_position(position);
                        }
                    }
                }
            }
        }
        self.window_picker.poll(&ctx);
        let explicit_changed = self.window_picker.take_display_changed();
        let previous_target = self.desktop_targets.key();
        if explicit_changed {
            self.desktop_targets.ack_target(
                self.window_picker.host_generation,
                self.window_picker.target_identity(),
            );
        }
        let automatic_changed = self.started
            && self.desktop_targets.poll(
                &self.api_host(),
                self.control_port.parse().unwrap_or(3391),
                &self.password,
            );
        if explicit_changed || automatic_changed {
            self.target_views
                .insert(previous_target, (self.viewport, self.fit_screen));
            if self.target_views.len() > 32 {
                self.target_views.clear();
            }
            let view = self.view_mode;
            self.close_capture();
            self.connect();
            if explicit_changed || matches!(view, ViewMode::Overview | ViewMode::FullScreen) {
                self.enter_fullscreen();
            }
            if automatic_changed {
                if let Some((viewport, fit)) =
                    self.target_views.get(&self.desktop_targets.key()).copied()
                {
                    self.viewport = viewport;
                    self.fit_screen = fit;
                } else {
                    self.fit_screen = true;
                    self.viewport.reset();
                }
            } else {
                self.fit_screen = true;
                self.viewport.reset();
            }
        }
        if self.applied_theme != Some(self.theme) {
            theme::apply(&ctx, self.theme.palette());
            self.applied_theme = Some(self.theme);
        }
        self.update_stream_visibility();
        let palette = self.theme.palette();
        ui.painter()
            .rect_filled(ui.max_rect(), 0.0, palette.background);
        ctx.request_repaint_after(Duration::from_millis(
            if self.view_mode == ViewMode::FullScreen {
                30
            } else {
                100
            },
        ));

        ui.vertical(|ui| {
            if !self.started {
                egui::ScrollArea::vertical().show(ui, |ui| {
                    self.draw_connection(ui, palette);
                });
                return;
            }
            let fullscreen = self.view_mode == ViewMode::FullScreen && self.started;
            if fullscreen
                && !self.show_keyboard
                && self.toolbar_visible
                && self
                    .toolbar_hide_at
                    .is_some_and(|deadline| Instant::now() >= deadline)
            {
                self.toolbar_visible = false;
            }
            if fullscreen && !self.toolbar_visible {
                self.draw_fullscreen_stream(&ctx, ui, palette);
                return;
            }
            if !matches!(self.view_mode, ViewMode::Terminal | ViewMode::Files) {
                self.draw_header(ui, palette);
            }
            if !self.started {
                return;
            }
            if !matches!(
                self.view_mode,
                ViewMode::FullScreen | ViewMode::Terminal | ViewMode::Files
            ) {
                self.draw_navigation(ui, palette);
            }
            if matches!(self.view_mode, ViewMode::Overview | ViewMode::FullScreen) {
                self.desktop_targets.ui(
                    ui,
                    &self.api_host(),
                    self.control_port.parse().unwrap_or(3391),
                    &self.password,
                );
            }
            if self.view_mode == ViewMode::Terminal {
                let mut content = ui.available_rect_before_wrap();
                #[cfg(target_os = "android")]
                if let Some(android) = &self.android_clipboard {
                    let bottom = android.visible_content_bottom();
                    if bottom > 0 {
                        content.max.y = content
                            .max
                            .y
                            .min(bottom as f32 / ctx.pixels_per_point())
                            .max(content.min.y + 1.0);
                    }
                }
                let previous_alignment = self.terminal.control_alignment;
                let previous_font_size = self.terminal.font_size();
                let actions = ui
                    .scope_builder(egui::UiBuilder::new().max_rect(content), |ui| {
                        self.terminal.ui(ui)
                    })
                    .inner;
                if self.terminal.control_alignment != previous_alignment
                    || self.terminal.font_size() != previous_font_size
                {
                    let mut settings = self.preferences.clone();
                    settings.control_alignment = self.terminal.control_alignment;
                    settings.terminal_font_size = self.terminal.font_size().round() as u8;
                    self.update_preferences(settings);
                }
                for action in actions {
                    match action {
                        terminal::TerminalAction::ShowKeyboard => {}
                        terminal::TerminalAction::Back => {
                            self.view_mode = ViewMode::Overview;
                        }
                        terminal::TerminalAction::Copy(text) => {
                            #[cfg(target_os = "android")]
                            if let Some(android) = &self.android_clipboard {
                                let _ = android.set_text(&text);
                            }
                            #[cfg(not(target_os = "android"))]
                            ctx.copy_text(text);
                        }
                        terminal::TerminalAction::Paste => {
                            #[cfg(target_os = "android")]
                            if let Some(android) = &self.android_clipboard {
                                if let Ok(text) = android.get_text() {
                                    self.terminal.paste(&text);
                                }
                            }
                        }
                        terminal::TerminalAction::AttachFile { working_directory } => {
                            self.file_browser.request_attach(&working_directory);
                        }
                    }
                }
                return;
            }
            if self.view_mode == ViewMode::Files {
                let mut back = false;
                if let Some(path) = self.file_browser.show(ui, &mut back) {
                    self.terminal.insert_path(&path);
                    self.view_mode = ViewMode::Terminal;
                }
                if back {
                    self.view_mode = ViewMode::Overview;
                }
                return;
            }
            if self.view_mode == ViewMode::Overview {
                self.draw_overview(ui, palette);
                return;
            }
            self.draw_toolbar(ui, palette);
            ui.add_space(4.0);
            if ctx.input(|input| input.pointer.any_click()) {
                self.toolbar_hide_at = Some(Instant::now() + TOOLBAR_AUTO_HIDE);
            }
            let mut available = ui.available_size();
            #[cfg(target_os = "android")]
            if self.show_keyboard {
                if let Some(android) = &self.android_clipboard {
                    let bottom = android.visible_content_bottom();
                    if bottom > 0 {
                        let visible = bottom as f32 / ctx.pixels_per_point() - ui.cursor().min.y;
                        available.y = available.y.min(visible.max(1.0));
                    }
                }
            }
            available.y = (available.y - 24.0).max(1.0);
            let (rect, response) = ui.allocate_exact_size(available, egui::Sense::click_and_drag());
            ui.painter().rect_filled(rect, 0.0, palette.background);
            self.draw_stream(&ctx, ui, palette, rect, response);
        });
        self.persist_current_view();
    }
}

/// Convert an RGBA8 byte buffer into egui's `Vec<Color32>` without per-pixel work.
///
/// `Color32` is a transparent `[u8; 4]` wrapper and the source buffer is
/// row-major RGBA8 with the exact length, so the allocation is reinterpreted.
/// Falls back to a plain conversion when the bytes are not a multiple of four.
fn cast_rgba8_to_color32(rgba8: Vec<u8>) -> Vec<Color32> {
    if rgba8.len() % 4 != 0 || rgba8.capacity() % 4 != 0 {
        return rgba8
            .chunks_exact(4)
            .map(|pixel| Color32::from_rgba_unmultiplied(pixel[0], pixel[1], pixel[2], pixel[3]))
            .collect();
    }
    let len = rgba8.len() / 4;
    let capacity = rgba8.capacity() / 4;
    let mut rgba8 = std::mem::ManuallyDrop::new(rgba8);
    // SAFETY: `Color32` has the same layout as `[u8; 4]`; the pointer stays valid
    // for `len` elements with `capacity` allocated and we never free the original.
    unsafe { Vec::from_raw_parts(rgba8.as_mut_ptr().cast::<Color32>(), len, capacity) }
}

#[cfg(test)]
mod frame_cast_tests {
    use super::*;

    #[test]
    fn rgba8_bytes_round_trip_through_color32() {
        // Desktop frames are fully opaque; for a=255, premultiplied == unmultiplied.
        let bytes: Vec<u8> = vec![1, 2, 3, 255, 255, 128, 0, 255];
        let pixels = cast_rgba8_to_color32(bytes);
        assert_eq!(pixels[0], Color32::from_rgba_unmultiplied(1, 2, 3, 255));
        assert_eq!(pixels[1], Color32::from_rgba_unmultiplied(255, 128, 0, 255));
    }
}

#[cfg(test)]
mod input_batch_tests {
    use super::*;
    use ironrdp_pdu::input::fast_path::KeyboardFlags;

    #[test]
    fn long_text_keeps_event_order_and_fits_protocol_packets() {
        let (tx, mut rx) = RdpInputSender::channel(1024);
        let mut app = WindowLinkApp::default();
        app.input = Some(tx);
        let expected: Vec<_> = (0..300_u16)
            .flat_map(|code| {
                [
                    FastPathInputEvent::UnicodeKeyboardEvent(KeyboardFlags::empty(), code),
                    FastPathInputEvent::UnicodeKeyboardEvent(KeyboardFlags::RELEASE, code),
                ]
            })
            .collect();
        app.send_operations(expected.clone());
        let mut received = Vec::new();
        while let Ok(RdpInputEvent::FastPath(events)) = rx.try_recv() {
            assert!(!events.is_empty() && events.len() <= 255);
            received.extend(events);
        }
        assert_eq!(received, expected);
    }
}

#[cfg(test)]
mod reconnect_tests {
    use super::*;

    #[test]
    fn backoff_retries_quickly_then_gives_up() {
        let mut backoff = ReconnectBackoff::new();
        assert_eq!(backoff.schedule(), Some(ReconnectBackoff::FIRST_DELAY));
        for _ in 0..(ReconnectBackoff::MAX_ATTEMPTS - 1) {
            assert_eq!(backoff.schedule(), Some(ReconnectBackoff::RETRY_DELAY));
        }
        assert_eq!(backoff.schedule(), None);
    }

    #[test]
    fn backoff_reset_restarts_the_schedule() {
        let mut backoff = ReconnectBackoff::new();
        for _ in 0..ReconnectBackoff::MAX_ATTEMPTS {
            assert!(backoff.schedule().is_some());
        }
        assert_eq!(backoff.schedule(), None);
        backoff.reset();
        assert_eq!(backoff.schedule(), Some(ReconnectBackoff::FIRST_DELAY));
    }
}

#[cfg(test)]
mod settings_menu_layout_tests {
    use super::settings_menu_max_height;

    #[test]
    fn settings_popup_scroll_area_stays_within_landscape_and_portrait_viewports() {
        for height in [360.0, 800.0] {
            let max_height = settings_menu_max_height(height);
            assert!(max_height < height);
            assert!(max_height > 0.0);
            assert!(height - max_height >= 32.0);
        }
    }
}
