//! Persistent tmux sessions. HTTP clients own only an attach PTY, never the shell.
use crate::control::{send_bytes, ApiError, Request};
use portable_pty::{native_pty_system, Child, CommandBuilder, MasterPty, PtySize};
use serde::{Deserialize, Serialize};
use std::{
    collections::{HashMap, VecDeque},
    io::{Read, Write},
    net::TcpStream,
    path::PathBuf,
    process::Command,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, Mutex, Weak,
    },
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

const PREFIX: &str = "windowlink-";
const OUTPUT_LIMIT: usize = 1024 * 1024;
const INPUT_LIMIT: usize = 64 * 1024;
const CLIENT_TIMEOUT: Duration = Duration::from_secs(30);
static NEXT_CLIENT: AtomicU64 = AtomicU64::new(1);

#[derive(Serialize, Debug)]
struct Session {
    id: String,
    name: String,
    working_directory: String,
    attached: bool,
}
#[derive(Default)]
struct Output {
    bytes: VecDeque<u8>,
    start: u64,
    ended: bool,
}
impl Output {
    fn push(&mut self, bytes: &[u8]) {
        self.bytes.extend(bytes);
        let drop = self.bytes.len().saturating_sub(OUTPUT_LIMIT);
        self.bytes.drain(..drop);
        self.start += drop as u64;
    }
}
struct Attachment {
    client: String,
    master: Box<dyn MasterPty + Send>,
    child: Box<dyn Child + Send + Sync>,
    writer: Box<dyn Write + Send>,
    output: Arc<Mutex<Output>>,
    last_seen: Instant,
    scroll_mode: bool,
    session_id: String,
    socket: Option<PathBuf>,
}
impl Drop for Attachment {
    fn drop(&mut self) {
        if self.scroll_mode {
            let mut command = Command::new("tmux");
            if let Some(socket) = &self.socket {
                command.arg("-S").arg(socket);
            }
            let _ = command
                .args(["send-keys", "-t", &self.session_id, "-X", "cancel"])
                .output();
        }
        // This is the tmux attach client, not the shell or tmux server.
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

pub(crate) struct TerminalService {
    attachments: Arc<Mutex<HashMap<String, Attachment>>>,
    // Tests use a separate server socket; production uses the user's normal tmux.
    socket: Option<PathBuf>,
}
impl Default for TerminalService {
    fn default() -> Self {
        Self::new(None)
    }
}
impl TerminalService {
    fn new(socket: Option<PathBuf>) -> Self {
        let attachments = Arc::new(Mutex::new(HashMap::<String, Attachment>::new()));
        let weak: Weak<Mutex<HashMap<String, Attachment>>> = Arc::downgrade(&attachments);
        thread::spawn(move || loop {
            thread::sleep(Duration::from_secs(5));
            let Some(attachments) = weak.upgrade() else {
                break;
            };
            if let Ok(mut attachments) = attachments.lock() {
                attachments.retain(|_, attachment| attachment.last_seen.elapsed() < CLIENT_TIMEOUT);
            };
        });
        Self {
            attachments,
            socket,
        }
    }
    fn command(&self) -> Command {
        let mut command = Command::new("tmux");
        if let Some(socket) = &self.socket {
            command.arg("-S").arg(socket);
        }
        command
    }
    fn tmux(&self, args: &[&str]) -> Result<String, ApiError> {
        let output = self
            .command()
            .args(args)
            .output()
            .map_err(|e| ApiError::new(503, format!("tmux is required for terminals: {e}")))?;
        if !output.status.success() {
            return Err(ApiError::new(
                409,
                String::from_utf8_lossy(&output.stderr).trim().to_owned(),
            ));
        }
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    }
    fn sessions(&self) -> Result<Vec<Session>, ApiError> {
        let output = self
            .command()
            .args([
                "list-sessions",
                "-F",
                "#{session_id}\t#{session_name}\t#{pane_current_path}\t#{session_attached}",
            ])
            .output()
            .map_err(|e| ApiError::new(503, format!("tmux is required for terminals: {e}")))?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            if stderr.contains("no server running")
                || stderr.contains("No such file")
                || stderr.contains("Connection refused")
            {
                return Ok(Vec::new());
            }
            return Err(ApiError::new(503, stderr.trim().to_owned()));
        }
        Ok(String::from_utf8_lossy(&output.stdout)
            .lines()
            .filter_map(|line| {
                let mut parts = line.splitn(4, '\t');
                let id = parts.next()?.to_owned();
                let name = parts.next()?.strip_prefix(PREFIX)?.to_owned();
                Some(Session {
                    id,
                    name,
                    working_directory: parts.next()?.to_owned(),
                    attached: parts.next()? != "0",
                })
            })
            .collect())
    }
    fn session(&self, id: &str) -> Result<Session, ApiError> {
        if !id.starts_with('$') || !id[1..].chars().all(|c| c.is_ascii_digit()) || id.len() < 2 {
            return Err(ApiError::new(400, "invalid terminal session identifier"));
        }
        self.sessions()?
            .into_iter()
            .find(|session| session.id == id)
            .ok_or_else(|| ApiError::new(404, "terminal session no longer exists"))
    }
    fn attach(&self, id: &str, size: Size) -> Result<String, ApiError> {
        self.session(id)?;
        let pair = native_pty_system()
            .openpty(size.pty())
            .map_err(|e| ApiError::new(500, e.to_string()))?;
        let mut command = CommandBuilder::new("tmux");
        command.arg("-u");
        if let Some(socket) = &self.socket {
            command.arg("-S");
            command.arg(socket);
        }
        command.args(["attach-session", "-t", id]);
        command.env("TERM", "xterm-256color");
        command.env("COLORTERM", "truecolor");
        // Parent may itself be inside tmux; this PTY is an independent client.
        command.env_remove("TMUX");
        let child = pair
            .slave
            .spawn_command(command)
            .map_err(|e| ApiError::new(500, e.to_string()))?;
        drop(pair.slave);
        let mut reader = pair
            .master
            .try_clone_reader()
            .map_err(|e| ApiError::new(500, e.to_string()))?;
        let writer = pair
            .master
            .take_writer()
            .map_err(|e| ApiError::new(500, e.to_string()))?;
        // Bound input writes as well as output; the HTTP worker cannot hang on a stopped PTY.
        if let Some(fd) = pair.master.as_raw_fd() {
            unsafe {
                let flags = libc::fcntl(fd, libc::F_GETFL);
                if flags >= 0 {
                    libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK);
                }
            }
        }
        let output = Arc::new(Mutex::new(Output::default()));
        let reader_output = output.clone();
        thread::spawn(move || {
            let mut buffer = [0; 16384];
            loop {
                match reader.read(&mut buffer) {
                    Ok(0) => break,
                    Ok(count) => {
                        if let Ok(mut output) = reader_output.lock() {
                            output.push(&buffer[..count]);
                        }
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(10));
                    }
                    Err(_) => break,
                }
            }
            if let Ok(mut output) = reader_output.lock() {
                output.ended = true;
            }
        });
        let client = format!(
            "{:x}-{:x}",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos(),
            NEXT_CLIENT.fetch_add(1, Ordering::Relaxed)
        );
        let attachment = Attachment {
            client: client.clone(),
            master: pair.master,
            child,
            writer,
            output,
            last_seen: Instant::now(),
            scroll_mode: false,
            session_id: id.to_owned(),
            socket: self.socket.clone(),
        };
        self.attachments
            .lock()
            .map_err(unavailable)?
            .insert(id.to_owned(), attachment);
        Ok(client)
    }
    pub(crate) fn route(
        &self,
        request: &Request,
        stream: &mut TcpStream,
    ) -> Option<Result<(), ApiError>> {
        let path = request.path.split('?').next().unwrap_or("");
        if !path.starts_with("/terminal/") {
            return None;
        }
        Some((|| {
            if path == "/terminal/sessions" {
                match request.method.as_str() {
                    "GET" => return json(stream, &self.sessions()?),
                    "POST" => {
                        #[derive(Deserialize)]
                        struct Create {
                            name: String,
                            #[serde(default)]
                            working_directory: String,
                        }
                        let create: Create = parse(request)?;
                        let name = session_name(&create.name)?;
                        let directory = if create.working_directory.is_empty() {
                            crate::config::home_dir()
                                .map_err(|e| ApiError::new(500, e))?
                                .to_string_lossy()
                                .into_owned()
                        } else {
                            create.working_directory
                        };
                        let directory = std::fs::canonicalize(&directory)
                            .map_err(|e| ApiError::new(400, format!("working folder: {e}")))?;
                        if !directory.is_dir() {
                            return Err(ApiError::new(400, "working folder is not a directory"));
                        }
                        let directory = directory
                            .to_str()
                            .ok_or_else(|| ApiError::new(400, "working folder must be UTF-8"))?;
                        let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/bash".into());
                        let id = self.tmux(&[
                            "new-session",
                            "-d",
                            "-P",
                            "-F",
                            "#{session_id}",
                            "-s",
                            &name,
                            "-c",
                            directory,
                            "env",
                            "TERM=tmux-256color",
                            &shell,
                        ])?;
                        // Enable modern terminal features and allow phone size to govern this client.
                        let _ = self.tmux(&[
                            "set-option",
                            "-t",
                            id.trim(),
                            "history-limit",
                            &crate::config::get().terminal_history_lines.to_string(),
                        ]);
                        let _ = self.tmux(&[
                            "set-option",
                            "-t",
                            id.trim(),
                            "default-terminal",
                            "tmux-256color",
                        ]);
                        let _ = self.tmux(&[
                            "set-window-option",
                            "-t",
                            id.trim(),
                            "window-size",
                            "latest",
                        ]);
                        let _ = self.tmux(&[
                            "set-option",
                            "-t",
                            id.trim(),
                            "terminal-features",
                            "xterm-256color:RGB",
                        ]);
                        return json(stream, &self.session(id.trim())?);
                    }
                    _ => return Err(ApiError::new(405, "unsupported terminal method")),
                }
            }
            let mut parts = path.trim_start_matches("/terminal/").split('/');
            let id = parts.next().unwrap_or("");
            let action = parts.next().unwrap_or("");
            if parts.next().is_some() {
                return Err(ApiError::new(404, "unknown terminal route"));
            }
            self.session(id)?;
            if request.method == "DELETE" && action.is_empty() {
                self.attachments.lock().map_err(unavailable)?.remove(id);
                self.tmux(&["kill-session", "-t", id])?;
                return empty(stream);
            }
            if request.method == "POST" && action == "rename" {
                #[derive(Deserialize)]
                struct Rename {
                    name: String,
                }
                self.tmux(&[
                    "rename-session",
                    "-t",
                    id,
                    &session_name(&parse::<Rename>(request)?.name)?,
                ])?;
                return json(stream, &self.session(id)?);
            }
            if request.method == "POST" && action == "attach" {
                let client = self.attach(id, parse::<Size>(request)?.validated()?)?;
                return json(stream, &serde_json::json!({"client":client,"cursor":0}));
            }
            let client = query(&request.path, "client")
                .ok_or_else(|| ApiError::new(400, "missing terminal client"))?;
            let mut attachments = self.attachments.lock().map_err(unavailable)?;
            let attachment = attachments
                .get_mut(id)
                .filter(|a| a.client == client)
                .ok_or_else(|| ApiError::new(409, "terminal detached; attach again"))?;
            attachment.last_seen = Instant::now();
            match (request.method.as_str(), action) {
                ("GET", "output") => {
                    let cursor: u64 = query(&request.path, "cursor")
                        .unwrap_or("0")
                        .parse()
                        .map_err(|_| ApiError::new(400, "invalid output cursor"))?;
                    let output = attachment.output.lock().map_err(unavailable)?;
                    let end = output.start + output.bytes.len() as u64;
                    let reset = cursor < output.start || cursor > end;
                    let bytes: Vec<u8> = if reset {
                        Vec::new()
                    } else {
                        output
                            .bytes
                            .iter()
                            .skip((cursor - output.start) as usize)
                            .take(64 * 1024)
                            .copied()
                            .collect()
                    };
                    json(
                        stream,
                        &serde_json::json!({"cursor":if reset {end} else {cursor+bytes.len() as u64},"bytes":bytes,"reset":reset,"ended":output.ended}),
                    )
                }
                ("POST", "input" | "response") => {
                    if request.body.len() > INPUT_LIMIT {
                        return Err(ApiError::new(413, "terminal input exceeds 64 KiB"));
                    }
                    // Window size is authoritative in TIOCGWINSZ. A queued
                    // response to an earlier resize can outlive tmux's query
                    // flag; tmux then interprets its numbers as shell keys.
                    if action == "response" && window_size_response(&request.body) {
                        return empty(stream);
                    }
                    if action == "input" && attachment.scroll_mode {
                        let _ = self.tmux(&["send-keys", "-t", id, "-X", "cancel"]);
                        attachment.scroll_mode = false;
                    }
                    let deadline = Instant::now() + Duration::from_secs(2);
                    let mut bytes = &request.body[..];
                    while !bytes.is_empty() {
                        match attachment.writer.write(bytes) {
                            Ok(0) => return Err(ApiError::new(409, "terminal input closed")),
                            Ok(n) => bytes = &bytes[n..],
                            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                            Err(e)
                                if e.kind() == std::io::ErrorKind::WouldBlock
                                    && Instant::now() < deadline =>
                            {
                                thread::sleep(Duration::from_millis(5))
                            }
                            Err(e) => {
                                return Err(ApiError::new(409, format!("terminal input: {e}")))
                            }
                        }
                    }
                    empty(stream)
                }
                ("POST", "resize") => {
                    attachment
                        .master
                        .resize(parse::<Size>(request)?.validated()?.pty())
                        .map_err(|e| ApiError::new(500, e.to_string()))?;
                    empty(stream)
                }
                ("POST", "scroll") => {
                    #[derive(Deserialize)]
                    struct Scroll {
                        lines: i32,
                    }
                    let lines = parse::<Scroll>(request)?.lines.clamp(-100, 100);
                    if lines > 0 && !attachment.scroll_mode {
                        self.tmux(&["copy-mode", "-t", id])?;
                        attachment.scroll_mode = true;
                    }
                    if attachment.scroll_mode && lines != 0 {
                        self.tmux(&[
                            "send-keys",
                            "-t",
                            id,
                            "-X",
                            "-N",
                            &lines.unsigned_abs().to_string(),
                            if lines > 0 {
                                "scroll-up"
                            } else {
                                "scroll-down"
                            },
                        ])?;
                    }
                    empty(stream)
                }
                ("POST", "detach") => {
                    attachments.remove(id);
                    empty(stream)
                }
                _ => Err(ApiError::new(404, "unknown terminal route")),
            }
        })())
    }
}
fn unavailable<E: std::fmt::Display>(error: E) -> ApiError {
    ApiError::new(500, format!("terminal state unavailable: {error}"))
}
#[derive(Deserialize, Clone, Copy)]
struct Size {
    cols: u16,
    rows: u16,
    #[serde(default)]
    pixel_width: u16,
    #[serde(default)]
    pixel_height: u16,
}
impl Size {
    fn validated(self) -> Result<Self, ApiError> {
        if !(2..=500).contains(&self.cols) || !(1..=300).contains(&self.rows) {
            return Err(ApiError::new(
                400,
                "terminal size outside 2–500 columns / 1–300 rows",
            ));
        }
        Ok(self)
    }
    fn pty(self) -> PtySize {
        PtySize {
            cols: self.cols,
            rows: self.rows,
            // Older clients omit pixels. Keep nonzero cell dimensions so
            // tmux never needs asynchronous window-size query replies.
            pixel_width: self.pixel_width.max(self.cols),
            pixel_height: self.pixel_height.max(self.rows),
        }
    }
}
fn window_size_response(bytes: &[u8]) -> bool {
    let Some(params) = bytes
        .strip_prefix(b"\x1b[")
        .and_then(|bytes| bytes.strip_suffix(b"t"))
    else {
        return false;
    };
    let mut fields = params.split(|byte| *byte == b';');
    matches!(fields.next(), Some(b"4" | b"8"))
        && fields.clone().count() == 2
        && fields.all(|field| !field.is_empty() && field.iter().all(u8::is_ascii_digit))
}
fn session_name(name: &str) -> Result<String, ApiError> {
    if name.is_empty()
        || name.len() > 80
        || name
            .chars()
            .any(|c| c.is_control() || matches!(c, '.' | ':' | '/' | '\\'))
    {
        return Err(ApiError::new(
            400,
            "name must be 1–80 characters without controls, dots, colons or slashes",
        ));
    }
    Ok(format!("{PREFIX}{name}"))
}
fn query<'a>(path: &'a str, key: &str) -> Option<&'a str> {
    path.split_once('?')?.1.split('&').find_map(|part| {
        let (k, v) = part.split_once('=')?;
        (k == key).then_some(v)
    })
}
fn parse<T: for<'a> Deserialize<'a>>(request: &Request) -> Result<T, ApiError> {
    serde_json::from_slice(&request.body)
        .map_err(|e| ApiError::new(400, format!("invalid terminal request: {e}")))
}
fn json(stream: &mut TcpStream, value: &impl Serialize) -> Result<(), ApiError> {
    let bytes = serde_json::to_vec(value).map_err(unavailable)?;
    send_bytes(stream, 200, "application/json", &bytes).map_err(unavailable)
}
fn empty(stream: &mut TcpStream) -> Result<(), ApiError> {
    send_bytes(stream, 204, "application/json", &[]).map_err(unavailable)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn post(service: &TerminalService, path: String, body: Vec<u8>) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let mut client = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        let (mut server, _) = listener.accept().unwrap();
        let request = Request {
            method: "POST".into(),
            path,
            headers: Vec::new(),
            content_length: body.len() as u64,
            body,
        };
        service.route(&request, &mut server).unwrap().unwrap();
        drop(server);
        let mut response = String::new();
        client.read_to_string(&mut response).unwrap();
        assert!(response.starts_with("HTTP/1.1 204"), "{response}");
    }
    #[test]
    fn bounded_output_requires_redraw_after_overflow() {
        let mut output = Output::default();
        output.push(&vec![b'x'; OUTPUT_LIMIT + 500]);
        assert_eq!(output.bytes.len(), OUTPUT_LIMIT);
        assert_eq!(output.start, 500);
    }
    #[test]
    fn tmux_shell_survives_detach_and_service_restart() {
        if Command::new("tmux").arg("-V").output().is_err() {
            return;
        }
        let directory = std::env::temp_dir().join(format!(
            "windowlink-terminal-test-{}-{}",
            std::process::id(),
            NEXT_CLIENT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir(&directory).unwrap();
        let socket = directory.join("tmux.sock");
        struct Cleanup(PathBuf, PathBuf);
        impl Drop for Cleanup {
            fn drop(&mut self) {
                let _ = Command::new("tmux")
                    .arg("-S")
                    .arg(&self.0)
                    .arg("kill-server")
                    .output();
                let _ = std::fs::remove_dir_all(&self.1);
            }
        }
        let _cleanup = Cleanup(socket.clone(), directory.clone());
        let service = TerminalService::new(Some(socket.clone()));
        let id = service
            .tmux(&[
                "new-session",
                "-d",
                "-P",
                "-F",
                "#{session_id}",
                "-s",
                "windowlink-test",
                "-c",
                directory.to_str().unwrap(),
                "sh",
            ])
            .unwrap()
            .trim()
            .to_owned();
        let client = service
            .attach(
                &id,
                Size {
                    cols: 80,
                    rows: 24,
                    pixel_width: 800,
                    pixel_height: 552,
                },
            )
            .unwrap();
        // A real renderer answers tmux's startup terminal queries. This test
        // waits out that short handshake before typing into its private shell.
        thread::sleep(Duration::from_millis(1100));
        let out = {
            let mut attachments = service.attachments.lock().unwrap();
            let attachment = attachments.get_mut(&id).unwrap();
            assert_eq!(attachment.client, client);
            attachment
                .writer
                .write_all(
                    b"printf 'window%s-proof\\n' link; sleep 1; printf 'after%s\\n' -detach\r",
                )
                .unwrap();
            attachment.output.clone()
        };
        let deadline = Instant::now() + Duration::from_secs(3);
        while Instant::now() < deadline {
            if String::from_utf8_lossy(
                &out.lock()
                    .unwrap()
                    .bytes
                    .iter()
                    .copied()
                    .collect::<Vec<_>>(),
            )
            .contains("windowlink-proof")
            {
                break;
            }
            thread::sleep(Duration::from_millis(20));
        }
        assert!(String::from_utf8_lossy(
            &out.lock()
                .unwrap()
                .bytes
                .iter()
                .copied()
                .collect::<Vec<_>>()
        )
        .contains("windowlink-proof"));
        service.attachments.lock().unwrap().remove(&id);
        drop(service);
        thread::sleep(Duration::from_millis(1200));
        let resumed = TerminalService::new(Some(socket));
        assert_eq!(resumed.sessions().unwrap()[0].id, id);
        assert_eq!(
            resumed.sessions().unwrap()[0].working_directory,
            directory.to_str().unwrap()
        );
        resumed
            .tmux(&["rename-session", "-t", &id, "windowlink-renamed"])
            .unwrap();
        assert_eq!(resumed.session(&id).unwrap().name, "renamed");
        let capture = resumed.tmux(&["capture-pane", "-p", "-t", &id]).unwrap();
        assert!(capture.contains("after-detach"));
        let client = resumed
            .attach(
                &id,
                Size {
                    cols: 100,
                    rows: 30,
                    pixel_width: 1000,
                    pixel_height: 690,
                },
            )
            .unwrap();
        // Rotate through portrait and landscape, including an older client
        // without pixel metadata. tmux must obtain pixels via TIOCGWINSZ,
        // rather than asking the phone and leaking delayed replies to sh.
        for body in [
            br#"{"cols":85,"rows":12,"pixel_width":1700,"pixel_height":552}"#.to_vec(),
            br#"{"cols":35,"rows":45}"#.to_vec(),
            br#"{"cols":85,"rows":12,"pixel_width":1700,"pixel_height":552}"#.to_vec(),
        ] {
            post(
                &resumed,
                format!("/terminal/{id}/resize?client={client}"),
                body,
            );
            thread::sleep(Duration::from_millis(100));
        }
        {
            let attachments = resumed.attachments.lock().unwrap();
            let attachment = attachments.get(&id).unwrap();
            let size = attachment.master.get_size().unwrap();
            assert_eq!(
                (size.cols, size.rows, size.pixel_width, size.pixel_height),
                (85, 12, 1700, 552)
            );
            let bytes: Vec<_> = attachment
                .output
                .lock()
                .unwrap()
                .bytes
                .iter()
                .copied()
                .collect();
            assert!(
                !bytes
                    .windows(5)
                    .any(|bytes| bytes == b"\x1b[18t" || bytes == b"\x1b[14t"),
                "tmux asked for window-size replies despite known PTY pixels"
            );
        }
        // Replay the exact responses that polluted the phone's shell after
        // rotation. They must be consumed, with no bytes written to the shell.
        let before = resumed.tmux(&["capture-pane", "-p", "-t", &id]).unwrap();
        for response in [b"\x1b[8;12;85t".as_slice(), b"\x1b[4;276;850t".as_slice()] {
            post(
                &resumed,
                format!("/terminal/{id}/response?client={client}"),
                response.to_vec(),
            );
        }
        thread::sleep(Duration::from_millis(100));
        assert_eq!(
            resumed.tmux(&["capture-pane", "-p", "-t", &id]).unwrap(),
            before,
            "stale size replies must not reach the shell"
        );
        post(
            &resumed,
            format!("/terminal/{id}/scroll?client={client}"),
            br#"{"lines":5}"#.to_vec(),
        );
        assert_eq!(
            resumed
                .tmux(&["display-message", "-p", "-t", &id, "#{pane_in_mode}"])
                .unwrap()
                .trim(),
            "1"
        );
        post(
            &resumed,
            format!("/terminal/{id}/response?client={client}"),
            b"\x1b[?6c".to_vec(),
        );
        assert_eq!(
            resumed
                .tmux(&["display-message", "-p", "-t", &id, "#{pane_in_mode}"])
                .unwrap()
                .trim(),
            "1",
            "terminal replies must not cancel history scrolling"
        );
        post(
            &resumed,
            format!("/terminal/{id}/input?client={client}"),
            b"\r".to_vec(),
        );
        assert_eq!(
            resumed
                .tmux(&["display-message", "-p", "-t", &id, "#{pane_in_mode}"])
                .unwrap()
                .trim(),
            "0"
        );
        post(
            &resumed,
            format!("/terminal/{id}/scroll?client={client}"),
            br#"{"lines":5}"#.to_vec(),
        );
        resumed.attachments.lock().unwrap().remove(&id);
        assert_eq!(
            resumed
                .tmux(&["display-message", "-p", "-t", &id, "#{pane_in_mode}"])
                .unwrap()
                .trim(),
            "0"
        );
        resumed.tmux(&["kill-session", "-t", &id]).unwrap();
        assert!(resumed.sessions().unwrap().is_empty());
    }
}
