mod clipboard;
mod config;
mod control;
mod desktop;
mod files;
mod terminal;

use serde::Serialize;
use std::{
    env, fs,
    io::{self, Read, Write},
    net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, TcpListener, TcpStream, UdpSocket},
    os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt},
    path::Path,
    process::{Child, Command, Stdio},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    thread,
    time::{Duration, Instant},
};

#[derive(Serialize)]
struct Endpoint<'a> {
    host: &'a str,
    port: u16,
    username: &'a str,
    password: &'a str,
    protocol: &'static str,
    view_mode: &'static str,
    window_address: Option<&'a str>,
    control_host: &'a str,
    control_port: u16,
    control_token: &'a str,
}

fn fail(message: impl std::fmt::Display) -> ! {
    eprintln!("windowlink-host: {message}");
    std::process::exit(1)
}
fn run_json(program: &str, args: &[&str]) -> Result<serde_json::Value, String> {
    let out = Command::new(program)
        .args(args)
        .output()
        .map_err(|e| format!("cannot run {program}: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "{program} {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    serde_json::from_slice(&out.stdout).map_err(|e| format!("invalid JSON from {program}: {e}"))
}
fn hyprland_check() -> Result<(), String> {
    if env::var_os("HYPRLAND_INSTANCE_SIGNATURE").is_none() {
        return Err("no Hyprland session detected (HYPRLAND_INSTANCE_SIGNATURE is unset)".into());
    }
    run_json("hyprctl", &["-j", "monitors"]).map(|_| ())
}
fn tailscale_ipv4() -> Option<Ipv4Addr> {
    let output = Command::new("tailscale").args(["ip", "-4"]).output().ok()?;
    if !output.status.success() {
        return None;
    }
    String::from_utf8(output.stdout).ok()?.trim().parse().ok()
}
fn hyprland_dispatch(expression: &str) -> Result<(), String> {
    let output = Command::new("hyprctl")
        .args(["dispatch", expression])
        .output()
        .map_err(|e| format!("cannot run hyprctl dispatch: {e}"))?;
    let response = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if !output.status.success() || !response.eq_ignore_ascii_case("ok") {
        return Err(if response.is_empty() {
            String::from_utf8_lossy(&output.stderr).trim().to_string()
        } else {
            response
        });
    }
    Ok(())
}
fn focus_window(address: &str) -> Result<(), String> {
    hyprland_dispatch(&format!(
        "hl.dsp.focus({{ window = \"address:{address}\" }})"
    ))
}
#[derive(Clone)]
struct WindowView {
    address: String,
    stable_id: String,
    output: String,
}
impl WindowView {
    fn inspect(address: &str) -> Result<Self, String> {
        let normalized = address.strip_prefix("0x").unwrap_or(address);
        if normalized.is_empty() || !normalized.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err("window address must be a hexadecimal Hyprland client address".into());
        }
        let address = format!("0x{}", normalized.to_ascii_lowercase());
        let clients = run_json("hyprctl", &["-j", "clients"])?;
        let client = clients
            .as_array()
            .and_then(|all| {
                all.iter().find(|client| {
                    client
                        .get("address")
                        .and_then(serde_json::Value::as_str)
                        .is_some_and(|candidate| candidate.eq_ignore_ascii_case(&address))
                })
            })
            .ok_or_else(|| format!("no Hyprland window found at {address}"))?;
        let stable_id = client
            .get("stableId")
            .and_then(serde_json::Value::as_str)
            .filter(|stable_id| !stable_id.is_empty())
            .ok_or_else(|| format!("Hyprland did not report a stable capture ID for {address}"))?
            .to_owned();
        let monitor_id = client
            .get("monitor")
            .and_then(serde_json::Value::as_i64)
            .ok_or_else(|| format!("Hyprland did not report a monitor for window {address}"))?;
        let monitors = run_json("hyprctl", &["-j", "monitors"])?;
        let output = monitors
            .as_array()
            .and_then(|all| {
                all.iter()
                    .find(|m| m.get("id").and_then(serde_json::Value::as_i64) == Some(monitor_id))
            })
            .and_then(|m| m.get("name").and_then(serde_json::Value::as_str))
            .ok_or_else(|| format!("could not find monitor {monitor_id} for window {address}"))?
            .to_owned();
        Ok(Self {
            address,
            stable_id,
            output,
        })
    }
}

fn spawn_hypr_rdp(
    config_path: &Path,
    target: Option<&WindowView>,
    monitor: Option<&str>,
) -> io::Result<Child> {
    let mut command = Command::new(&config::get().backend);
    command
        .arg("--config")
        .arg(config_path)
        .stdin(Stdio::null())
        .stdout(Stdio::from(io::stderr()));
    if let Some(monitor) = monitor.filter(|_| target.is_none()) {
        command.arg("--output").arg(monitor);
    }
    if let Some(target) = target {
        command
            .arg("--output")
            .arg(&target.output)
            .arg("--toplevel")
            .arg(&target.stable_id)
            .arg("--input-window")
            .arg(&target.address);
    }
    command.spawn()
}

fn wait_for_listener(
    child: &mut Child,
    socket: SocketAddr,
    stopping: &AtomicBool,
) -> Result<(), String> {
    let address = if socket.ip().is_unspecified() {
        SocketAddr::new(
            if socket.is_ipv6() {
                IpAddr::V6(Ipv6Addr::LOCALHOST)
            } else {
                IpAddr::V4(Ipv4Addr::LOCALHOST)
            },
            socket.port(),
        )
    } else {
        socket
    };
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if stopping.load(Ordering::SeqCst) {
            return Err("host is stopping".into());
        }
        match child.try_wait() {
            Ok(Some(status)) => return Err(format!("hypr-rdp exited during restart ({status})")),
            Ok(None) => {}
            Err(error) => return Err(format!("could not check hypr-rdp startup: {error}")),
        }
        if let Ok(probe) = TcpStream::connect_timeout(&address, Duration::from_millis(100)) {
            drop(probe);
            return match child.try_wait() {
                Ok(None) => Ok(()),
                Ok(Some(status)) => Err(format!("hypr-rdp exited during restart ({status})")),
                Err(error) => Err(format!("could not check hypr-rdp startup: {error}")),
            };
        }
        if Instant::now() >= deadline {
            return Err("timed out waiting for hypr-rdp to listen".into());
        }
        thread::sleep(Duration::from_millis(50));
    }
}

fn random_password() -> io::Result<String> {
    let mut bytes = [0u8; 24];
    fs::File::open("/dev/urandom")?.read_exact(&mut bytes)?;
    const ALPHABET: &[u8] = b"ABCDEFGHJKLMNPQRSTUVWXYZabcdefghijkmnopqrstuvwxyz23456789-_";
    Ok(bytes
        .iter()
        .map(|b| ALPHABET[*b as usize % ALPHABET.len()] as char)
        .collect())
}
fn persistent_password(path: &Path) -> io::Result<String> {
    match fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
    {
        Ok(mut file) => {
            let password = random_password()?;
            if let Err(error) = file.write_all(password.as_bytes()) {
                let _ = fs::remove_file(path);
                return Err(error);
            }
            file.sync_all()?;
            Ok(password)
        }
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
            let file = fs::OpenOptions::new()
                .read(true)
                .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
                .open(path)?;
            let metadata = file.metadata()?;
            if !metadata.is_file() || metadata.uid() != unsafe { libc::geteuid() } {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "persistent password must be a regular file owned by the current user",
                ));
            }
            file.set_permissions(fs::Permissions::from_mode(0o600))?;
            if metadata.len() > 4096 {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "persistent password exceeds 4096 bytes",
                ));
            }
            let mut password = String::new();
            file.take(4097).read_to_string(&mut password)?;
            if password.len() > 4096 {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "persistent password exceeds 4096 bytes",
                ));
            }
            parse_password_file(password)
        }
        Err(error) => Err(error),
    }
}

// Match hypr-rdp's removal of exactly one trailing LF or CRLF. The shared
// secret is also an HTTP bearer token, so additional lines and surrounding
// whitespace must fail before either listener starts serving requests.
fn parse_password_file(mut password: String) -> io::Result<String> {
    if password.ends_with('\n') {
        password.pop();
        if password.ends_with('\r') {
            password.pop();
        }
    }
    if password.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "persistent password file is empty",
        ));
    }
    if password.chars().any(char::is_control) || password.trim() != password {
        return Err(io::Error::new(io::ErrorKind::InvalidData,
            "persistent password must be one line without control characters or surrounding whitespace"));
    }
    Ok(password)
}

#[cfg(test)]
mod password_tests {
    use super::parse_password_file;

    #[test]
    fn password_file_accepts_one_optional_line_ending() {
        for input in ["secret", "secret\n", "secret\r\n", "with internal space\n"] {
            let expected = input
                .strip_suffix("\r\n")
                .or_else(|| input.strip_suffix('\n'))
                .unwrap_or(input);
            assert_eq!(parse_password_file(input.into()).unwrap(), expected);
        }
    }

    #[test]
    fn password_file_rejects_unusable_bearer_tokens() {
        for input in [
            "",
            "\n",
            "\r\n",
            "secret\n\n",
            "secret\r",
            "secret\r\n\r\n",
            "two\nlines",
            " secret",
            "secret ",
            "secret\t",
            "secret\0",
        ] {
            assert!(parse_password_file(input.into()).is_err(), "{input:?}");
        }
    }
}

fn write_private_file(path: &Path, contents: &[u8]) -> io::Result<()> {
    let temp_path = path.with_extension(format!("tmp-{}", std::process::id()));
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&temp_path)?;
    if let Err(error) = file.write_all(contents).and_then(|()| file.sync_all()) {
        let _ = fs::remove_file(&temp_path);
        return Err(error);
    }
    drop(file);
    if let Err(error) = fs::rename(&temp_path, path) {
        let _ = fs::remove_file(&temp_path);
        return Err(error);
    }
    Ok(())
}
fn usage() -> ! {
    eprintln!(
        "Usage:\n  windowlink-host check [--config FILE] [--backend PATH]\n  windowlink-host outputs\n  windowlink-host windows\n  windowlink-host focus <window-address>\n  windowlink-host serve [OPTIONS]\n\nOptions: --config FILE, --bind IP:PORT | --tailscale, --control-bind IP,\n  --control-port PORT, --username NAME, --backend PATH, --output NAME | --window ADDRESS,\n  --runtime-dir DIR, --password-file FILE, --endpoint-file FILE, --fps N, --bitrate BPS,\n  --text-clipboard-max-bytes N, --image-clipboard-max-bytes N,\n  --transfer-chunk-max-bytes N, --terminal-history-lines N.\n\nConfiguration: $XDG_CONFIG_HOME/omalink/host.json (default ~/.config/omalink/host.json).\nCLI options override JSON settings. Defaults: RDP 0.0.0.0:3389, Tailscale-only control\non 3391. Set --control-bind explicitly for LAN or loopback without Tailscale.\nCapture requires Linux with Hyprland and the patched hypr-rdp backend."
    );
    std::process::exit(2)
}
fn main() {
    let mut raw_args = env::args().skip(1).collect::<Vec<_>>();
    // Accept --config before the command as well as after it.
    if raw_args.first().is_some_and(|arg| arg == "--config") {
        if raw_args.len() < 3 {
            usage();
        }
        let global = raw_args.drain(..2).collect::<Vec<_>>();
        raw_args.extend(global);
    }
    let mut args = raw_args.into_iter();
    let Some(command) = args.next() else { usage() };
    match command.as_str() {
        "check" => {
            let settings = config::HostConfig::from_args(&args.collect::<Vec<_>>())
                .unwrap_or_else(|e| fail(e));
            let mut errors = Vec::new();
            let hyprland = match hyprland_check() {
                Ok(()) => true,
                Err(error) => {
                    errors.push(error);
                    false
                }
            };
            let backend = match Command::new(&settings.backend).arg("--help").output() {
                Ok(output) if output.status.success() => true,
                Ok(output) => {
                    errors.push(format!(
                        "backend {} --help failed: {}",
                        settings.backend.display(),
                        String::from_utf8_lossy(&output.stderr).trim()
                    ));
                    false
                }
                Err(error) => {
                    errors.push(format!(
                        "backend {} is unavailable: {error}",
                        settings.backend.display()
                    ));
                    false
                }
            };
            let capture_available = hyprland && backend;
            let portal = Command::new("busctl")
                .args([
                    "--user",
                    "get-property",
                    "org.freedesktop.portal.Desktop",
                    "/org/freedesktop/portal/desktop",
                    "org.freedesktop.portal.ScreenCast",
                    "AvailableSourceTypes",
                ])
                .output();
            let source_types = portal
                .ok()
                .filter(|o| o.status.success())
                .and_then(|o| String::from_utf8(o.stdout).ok())
                .and_then(|s| {
                    s.split_whitespace()
                        .last()
                        .and_then(|v| v.parse::<u32>().ok())
                })
                .unwrap_or(0);
            let window_portal = source_types & 2 != 0;
            let remote_desktop_portal = Command::new("busctl")
                .args([
                    "--user",
                    "introspect",
                    "org.freedesktop.portal.Desktop",
                    "/org/freedesktop/portal/desktop",
                ])
                .output()
                .is_ok_and(|o| {
                    o.status.success()
                        && String::from_utf8_lossy(&o.stdout)
                            .contains("org.freedesktop.portal.RemoteDesktop")
                });
            println!(
                "{}",
                serde_json::json!({
                    "hyprland": hyprland,
                    "hypr_rdp": backend,
                    "rdp_control": capture_available,
                    "window_picker": hyprland,
                    "host_window_selection": capture_available,
                    "window_capture": capture_available,
                    "capture_platform": "linux-hyprland",
                    "backend": settings.backend,
                    "errors": errors,
                    "portal_window_source": window_portal,
                    "remote_desktop_portal": remote_desktop_portal,
                    "tailscale_ipv4": tailscale_ipv4().map(|ip| ip.to_string()),
                    "notes": [
                        "--window captures only the selected window",
                        "Window selections restart the encoder without stopping control services",
                        "Input is injected through Hyprland by hypr-rdp"
                    ]
                })
            );
            if !capture_available {
                std::process::exit(1);
            }
        }
        "outputs" => {
            if let Err(e) = hyprland_check() {
                fail(e);
            }
            println!(
                "{}",
                run_json("hyprctl", &["-j", "monitors"]).unwrap_or_else(|e| fail(e))
            );
        }
        "windows" => {
            if let Err(e) = hyprland_check() {
                fail(e);
            }
            println!(
                "{}",
                run_json("hyprctl", &["-j", "clients"]).unwrap_or_else(|e| fail(e))
            );
        }
        "focus" => {
            if let Err(e) = hyprland_check() {
                fail(e);
            }
            let Some(address) = args.next() else { usage() };
            let normalized = address.strip_prefix("0x").unwrap_or(&address);
            if normalized.is_empty() || !normalized.bytes().all(|b| b.is_ascii_hexdigit()) {
                fail("window address must be a hexadecimal Hyprland client address");
            }
            let address = format!("0x{}", normalized.to_ascii_lowercase());
            focus_window(&address).unwrap_or_else(|e| fail(e));
            println!("ok");
        }
        "serve" => {
            let settings = config::HostConfig::from_args(&args.collect::<Vec<_>>())
                .unwrap_or_else(|e| fail(e));
            config::initialize(settings).unwrap_or_else(|e| fail(e));
            let settings = config::get();
            if let Err(e) = hyprland_check() {
                fail(e);
            }
            let mut socket = settings.bind;
            if settings.tailscale {
                let ip = tailscale_ipv4().unwrap_or_else(|| {
                    fail("could not get a Tailscale IPv4 address; connect Tailscale first")
                });
                socket.set_ip(ip.into());
            }
            let bind = socket.to_string();
            let port = socket.port();
            let control_port = settings.control_port;
            let control_ip = settings.control_bind.unwrap_or_else(|| {
                tailscale_ipv4().map(Into::into).unwrap_or_else(|| {
                    fail("control API requires Tailscale by default; connect Tailscale or configure --control-bind IP")
                })
            });
            let control_socket = SocketAddr::new(control_ip, control_port);
            let runtime = settings.runtime_dir().unwrap_or_else(|e| fail(e));
            let persistent_password_path = settings
                .password_file
                .clone()
                .unwrap_or_else(|| runtime.join("windowlink-hypr-rdp.password"));
            let endpoint_path = settings
                .endpoint_file
                .clone()
                .unwrap_or_else(|| runtime.join("windowlink-main-endpoint.json"));
            if persistent_password_path == endpoint_path {
                fail("password and endpoint paths must be different");
            }
            let control_listener = TcpListener::bind(control_socket).unwrap_or_else(|e| {
                fail(format!("cannot bind control API at {control_socket}: {e}"))
            });
            control_listener
                .set_nonblocking(true)
                .unwrap_or_else(|e| fail(format!("cannot configure control API listener: {e}")));
            TcpListener::bind(socket).unwrap_or_else(|e| {
                fail(format!(
                    "cannot bind {socket}: {e}; check for another server or unavailable interface"
                ))
            });
            let username = settings.username.as_str();
            let _ = fs::remove_file(&endpoint_path);
            let password = persistent_password(&persistent_password_path).unwrap_or_else(|e| {
                fail(format!(
                    "cannot load or create private session password: {e}"
                ))
            });
            let window_address = settings.window.clone();
            let output = settings.output.clone();
            let initial_window = window_address
                .as_deref()
                .map(WindowView::inspect)
                .transpose()
                .unwrap_or_else(|e| fail(e));
            let selected_output = initial_window
                .as_ref()
                .map(|view| view.output.clone())
                .or(output)
                .or_else(|| {
                    run_json("hyprctl", &["-j", "monitors"])
                        .ok()?
                        .as_array()?
                        .iter()
                        .find(|m| {
                            m.get("focused").and_then(serde_json::Value::as_bool) == Some(true)
                        })?
                        .get("name")?
                        .as_str()
                        .map(str::to_owned)
                });
            if selected_output.is_none() {
                fail("could not determine the Hyprland output to share");
            }
            let config_path =
                runtime.join(format!("windowlink-hypr-rdp-{}.toml", std::process::id()));
            let mut config = format!(
                "bind = {}\nusername = {}\npassword_file = {}\nfps = {}\nbitrate = {}\n",
                config::toml_string(&bind),
                config::toml_string(username),
                config::toml_string(persistent_password_path.to_str().unwrap()),
                settings.fps,
                settings.bitrate,
            );
            if let Some(ref name) = selected_output {
                config.push_str(&format!("output = {}\n", config::toml_string(name)));
            }
            fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&config_path)
                .and_then(|mut f| {
                    use std::io::Write;
                    f.write_all(config.as_bytes())
                })
                .unwrap_or_else(|e| fail(format!("cannot create private server config: {e}")));
            // Keep the launcher alive for Ctrl-C cleanup; the terminal also signals hypr-rdp directly.
            let interrupted = Arc::new(AtomicBool::new(false));
            let interrupted_handler = Arc::clone(&interrupted);
            if let Err(error) =
                ctrlc::set_handler(move || interrupted_handler.store(true, Ordering::SeqCst))
            {
                let _ = fs::remove_file(&config_path);
                fail(format!("cannot install Ctrl-C handler: {error}"));
            }
            let child_result = spawn_hypr_rdp(
                &config_path,
                initial_window.as_ref(),
                selected_output.as_deref(),
            );
            let mut child = child_result.unwrap_or_else(|e| {
                let _ = fs::remove_file(&config_path);
                fail(format!(
                    "cannot start hypr-rdp (install hypr-rdp first): {e}"
                ))
            });
            // Wait briefly to catch argument/configuration errors before reporting a usable endpoint.
            let startup_deadline = Instant::now() + Duration::from_millis(600);
            loop {
                if interrupted.load(Ordering::SeqCst) {
                    let _ = child.kill();
                    let _ = child.wait();
                    let _ = fs::remove_file(&config_path);
                    fail("interrupted during startup");
                }
                match child.try_wait() {
                    Ok(Some(status)) => {
                        let _ = fs::remove_file(&config_path);
                        fail(format!("hypr-rdp exited during startup ({status})"));
                    }
                    Err(error) => {
                        let _ = child.kill();
                        let _ = child.wait();
                        let _ = fs::remove_file(&config_path);
                        fail(format!("could not check hypr-rdp startup: {error}"));
                    }
                    Ok(None) => {}
                }
                if Instant::now() >= startup_deadline {
                    break;
                }
                thread::sleep(Duration::from_millis(50));
            }
            let routed_ip = if !socket.ip().is_unspecified() {
                socket.ip().to_string()
            } else if let Some(ip) = tailscale_ipv4() {
                ip.to_string()
            } else {
                UdpSocket::bind("0.0.0.0:0")
                    .ok()
                    .and_then(|s| {
                        s.connect("192.0.2.1:9").ok()?;
                        s.local_addr().ok().map(|a| a.ip().to_string())
                    })
                    .unwrap_or_else(|| "<computer-LAN-IP>".into())
            };
            let view_mode = if initial_window.is_some() {
                "window"
            } else {
                "monitor"
            };
            let endpoint_window_address = initial_window.as_ref().map(|view| view.address.clone());
            let target_generation = Arc::new(std::sync::atomic::AtomicU64::new(0));
            let restart_acks = control::RestartAcks::default();
            let control_state = Arc::new(Mutex::new(control::WindowControl::new(
                initial_window.clone(),
                Arc::clone(&target_generation),
                restart_acks.clone(),
                selected_output.clone(),
            )));
            let control_state_thread = Arc::clone(&control_state);
            let control_stop = Arc::clone(&interrupted);
            let control_token = password.clone();
            let control_thread = thread::spawn(move || {
                control::run(
                    control_listener,
                    control_state_thread,
                    control_token,
                    control_stop,
                )
            });
            let endpoint = serde_json::to_string(&Endpoint {
                host: &routed_ip,
                port,
                username,
                password: &password,
                protocol: "rdp",
                view_mode,
                window_address: endpoint_window_address.as_deref(),
                control_host: &if control_ip.is_unspecified() {
                    routed_ip.clone()
                } else {
                    control_ip.to_string()
                },
                control_port,
                control_token: &password,
            })
            .unwrap();
            if let Err(error) = write_private_file(&endpoint_path, endpoint.as_bytes()) {
                let _ = child.kill();
                let _ = child.wait();
                let _ = fs::remove_file(&config_path);
                fail(format!("cannot write private endpoint file: {error}"));
            }
            let endpoint_write = {
                let mut stdout = io::stdout().lock();
                writeln!(stdout, "{endpoint}").and_then(|()| stdout.flush())
            };
            if let Err(error) = endpoint_write {
                let _ = child.kill();
                let _ = child.wait();
                interrupted.store(true, Ordering::SeqCst);
                let _ = control_thread.join();
                let _ = fs::remove_file(&config_path);
                let _ = fs::remove_file(&endpoint_path);
                fail(format!("could not write connection details: {error}"));
            }
            eprintln!(
                "Serving via hypr-rdp{}{}; stop with Ctrl-C.",
                endpoint_window_address
                    .as_ref()
                    .map(|address| format!(" capturing window {address}"))
                    .unwrap_or_default(),
                selected_output
                    .as_deref()
                    .map(|n| format!(" on output {n}"))
                    .unwrap_or_default()
            );
            let mut active_generation = target_generation.load(Ordering::SeqCst);
            let mut active_target = initial_window;
            let mut child = Some(child);
            let mut exit_failure = None;
            loop {
                if interrupted.load(Ordering::SeqCst) {
                    if let Some(child) = child.as_mut() {
                        let _ = child.kill();
                    }
                    break;
                }
                let requested_generation = target_generation.load(Ordering::SeqCst);
                if requested_generation != active_generation {
                    if let Some(mut old_child) = child.take() {
                        let _ = old_child.kill();
                        let _ = old_child.wait();
                    }
                    active_target = control_state
                        .lock()
                        .ok()
                        .and_then(|state| state.selected_target());
                    active_generation = requested_generation;
                    let monitor = control_state
                        .lock()
                        .ok()
                        .and_then(|state| state.selected_monitor());
                    match spawn_hypr_rdp(&config_path, active_target.as_ref(), monitor.as_deref()) {
                        Ok(mut new_child) => {
                            match wait_for_listener(&mut new_child, socket, &interrupted) {
                                Ok(()) => {
                                    child = Some(new_child);
                                    restart_acks.complete(requested_generation, Ok(()));
                                }
                                Err(error) => {
                                    let _ = new_child.kill();
                                    let _ = new_child.wait();
                                    eprintln!(
                                        "windowlink-host: selected capture did not start: {error}"
                                    );
                                    restart_acks.complete(requested_generation, Err(error));
                                }
                            }
                        }
                        Err(error) => {
                            let error =
                                format!("could not restart hypr-rdp for selection: {error}");
                            eprintln!("windowlink-host: {error}");
                            restart_acks.complete(requested_generation, Err(error));
                        }
                    }
                    // This is the only expected delay while the RDP listener is
                    // rebound; the authenticated control API remains available.
                    thread::sleep(Duration::from_millis(100));
                    continue;
                }
                if let Some(active_child) = child.as_mut() {
                    match active_child.try_wait() {
                        Ok(Some(status)) if active_target.is_some() => {
                            restart_acks.complete(
                                active_generation,
                                Err(format!("Selected capture stopped: {status}")),
                            );
                            eprintln!(
                                "windowlink-host: selected-window capture stopped ({status}); waiting for another window selection"
                            );
                            child = None;
                        }
                        Ok(Some(status)) => {
                            exit_failure = Some(status);
                            break;
                        }
                        Ok(None) => {}
                        Err(error) => {
                            eprintln!("windowlink-host: could not check hypr-rdp: {error}");
                            if active_target.is_some() {
                                restart_acks.complete(
                                    active_generation,
                                    Err(format!("Capture status unavailable: {error}")),
                                );
                                child = None;
                            } else {
                                break;
                            }
                        }
                    }
                }
                thread::sleep(Duration::from_millis(100));
            }
            interrupted.store(true, Ordering::SeqCst);
            let _ = control_thread.join();
            if let Some(mut child) = child {
                let _ = child.kill();
                let _ = child.wait();
            }
            let _ = fs::remove_file(&config_path);
            let _ = fs::remove_file(&endpoint_path);
            if let Some(status) = exit_failure.filter(|status| !status.success()) {
                fail(format!("hypr-rdp stopped with {status}"));
            }
        }
        _ => usage(),
    }
}
