//! Validated host settings: defaults < JSON configuration < command line.
use serde::Deserialize;
use std::{
    env, fs,
    net::{IpAddr, SocketAddr},
    os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt},
    path::PathBuf,
    sync::OnceLock,
};

#[derive(Clone, Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct HostConfig {
    pub bind: SocketAddr,
    pub tailscale: bool,
    pub control_bind: Option<IpAddr>,
    pub control_port: u16,
    pub username: String,
    pub backend: PathBuf,
    pub output: Option<String>,
    pub window: Option<String>,
    pub runtime_dir: Option<PathBuf>,
    pub password_file: Option<PathBuf>,
    pub endpoint_file: Option<PathBuf>,
    pub fps: u32,
    pub bitrate: u32,
    pub text_clipboard_max_bytes: usize,
    pub image_clipboard_max_bytes: usize,
    pub transfer_chunk_max_bytes: usize,
    pub terminal_history_lines: u32,
}
impl Default for HostConfig {
    fn default() -> Self {
        Self {
            bind: "0.0.0.0:3389".parse().unwrap(),
            tailscale: false,
            control_bind: None,
            control_port: 3391,
            username: "windowlink".into(),
            backend: "hypr-rdp".into(),
            output: None,
            window: None,
            runtime_dir: None,
            password_file: None,
            endpoint_file: None,
            fps: 30,
            bitrate: 10_000_000,
            text_clipboard_max_bytes: 1024 * 1024,
            image_clipboard_max_bytes: 32 * 1024 * 1024,
            transfer_chunk_max_bytes: 4 * 1024 * 1024,
            terminal_history_lines: 10_000,
        }
    }
}
static CONFIG: OnceLock<HostConfig> = OnceLock::new();
pub fn get() -> &'static HostConfig {
    CONFIG.get_or_init(HostConfig::default)
}
pub fn initialize(config: HostConfig) -> Result<(), String> {
    CONFIG
        .set(config)
        .map_err(|_| "host configuration already initialized".into())
}

pub fn home_dir() -> Result<PathBuf, String> {
    if let Some(home) = env::var_os("HOME").filter(|v| !v.is_empty()) {
        let path = PathBuf::from(home);
        if path.is_absolute() {
            return Ok(path);
        }
    }
    // Use the desktop user's account entry when HOME is absent (e.g. a service).
    let mut storage = vec![0u8; 65536];
    let mut entry = std::mem::MaybeUninit::<libc::passwd>::uninit();
    let mut result = std::ptr::null_mut();
    unsafe {
        if libc::getpwuid_r(
            libc::geteuid(),
            entry.as_mut_ptr(),
            storage.as_mut_ptr().cast(),
            storage.len(),
            &mut result,
        ) == 0
            && !result.is_null()
        {
            use std::os::unix::ffi::OsStrExt;
            let home = std::ffi::CStr::from_ptr((*result).pw_dir);
            let path = PathBuf::from(std::ffi::OsStr::from_bytes(home.to_bytes()));
            if path.is_absolute() {
                return Ok(path);
            }
        }
    }
    Err("cannot determine user home; set HOME to an absolute directory".into())
}

pub fn default_config_path() -> Result<PathBuf, String> {
    let base = match env::var_os("XDG_CONFIG_HOME").filter(|v| !v.is_empty()) {
        Some(path) => PathBuf::from(path),
        None => home_dir()?.join(".config"),
    };
    if !base.is_absolute() {
        return Err("XDG_CONFIG_HOME must be absolute".into());
    }
    Ok(base.join("omaviewer/host.json"))
}

impl HostConfig {
    pub fn from_args(args: &[String]) -> Result<Self, String> {
        let mut config_path = None;
        let mut options = Vec::new();
        let mut args = args.iter();
        while let Some(arg) = args.next() {
            if arg == "--config" {
                if config_path.is_some() {
                    return Err("--config specified more than once".into());
                }
                config_path = Some(PathBuf::from(
                    args.next().ok_or("--config requires a path")?,
                ));
            } else {
                options.push(arg.clone());
            }
        }
        let explicit = config_path.is_some();
        let path = match config_path {
            Some(path) => path,
            None => default_config_path()?,
        };
        let config = match fs::read(&path) {
            Ok(bytes) => serde_json::from_slice(&bytes)
                .map_err(|e| format!("invalid host config {}: {e}", path.display()))?,
            Err(e) if !explicit && e.kind() == std::io::ErrorKind::NotFound => Self::default(),
            Err(e) => return Err(format!("cannot read host config {}: {e}", path.display())),
        };
        Self::apply_options(config, &options)
    }

    fn apply_options(mut self, options: &[String]) -> Result<Self, String> {
        let mut args = options.iter();
        let mut bind_seen = false;
        let mut tailscale_seen = false;
        let mut output_seen = false;
        let mut window_seen = false;
        while let Some(arg) = args.next() {
            if arg == "--tailscale" {
                self.tailscale = true;
                tailscale_seen = true;
                continue;
            }
            let value = args
                .next()
                .ok_or_else(|| format!("{arg} requires a value"))?;
            macro_rules! number {
                ($field:ident) => {
                    self.$field = value
                        .parse()
                        .map_err(|_| format!("invalid value for {arg}: {value}"))?
                };
            }
            match arg.as_str() {
                "--bind" => {
                    number!(bind);
                    self.tailscale = false;
                    bind_seen = true;
                }
                "--control-bind" => {
                    self.control_bind = Some(
                        value
                            .parse()
                            .map_err(|_| "--control-bind requires an IP address (without port)")?,
                    )
                }
                "--control-port" => number!(control_port),
                "--username" => self.username = value.clone(),
                "--backend" => self.backend = value.into(),
                "--output" => {
                    self.output = Some(value.clone());
                    self.window = None;
                    output_seen = true;
                }
                "--window" => {
                    self.window = Some(value.clone());
                    self.output = None;
                    window_seen = true;
                }
                "--runtime-dir" => self.runtime_dir = Some(value.into()),
                "--password-file" => self.password_file = Some(value.into()),
                "--endpoint-file" => self.endpoint_file = Some(value.into()),
                "--fps" => number!(fps),
                "--bitrate" => number!(bitrate),
                "--text-clipboard-max-bytes" => number!(text_clipboard_max_bytes),
                "--image-clipboard-max-bytes" => number!(image_clipboard_max_bytes),
                "--transfer-chunk-max-bytes" => number!(transfer_chunk_max_bytes),
                "--terminal-history-lines" => number!(terminal_history_lines),
                _ => return Err(format!("unknown option: {arg}")),
            }
        }
        if bind_seen && tailscale_seen {
            return Err("use either --tailscale or --bind, not both".into());
        }
        if output_seen && window_seen {
            return Err("use either --window or --output, not both".into());
        }
        self.validate()?;
        Ok(self)
    }

    pub fn validate(&self) -> Result<(), String> {
        if self.bind.port() == 0 || self.control_port == 0 || self.bind.port() == self.control_port
        {
            return Err("RDP and control ports must be nonzero and different".into());
        }
        if self.username.trim().is_empty() || self.username.chars().any(char::is_control) {
            return Err("username must be nonempty and contain no control characters".into());
        }
        if self.backend.as_os_str().is_empty() {
            return Err("backend must name an executable or path".into());
        }
        if self.output.is_some() && self.window.is_some() {
            return Err("configure either output or window, not both".into());
        }
        if let Some(window) = &self.window {
            let address = window.strip_prefix("0x").unwrap_or(window);
            if address.is_empty() || !address.bytes().all(|b| b.is_ascii_hexdigit()) {
                return Err("window must be a hexadecimal Hyprland address".into());
            }
        }
        if self
            .output
            .as_ref()
            .is_some_and(|value| value.trim().is_empty())
        {
            return Err("output must not be empty".into());
        }
        if !(1..=240).contains(&self.fps) {
            return Err("fps must be between 1 and 240".into());
        }
        if !(1..=1_000_000_000).contains(&self.bitrate) {
            return Err("bitrate must be between 1 and 1000000000 bits/second".into());
        }
        for (name, value) in [
            ("text_clipboard_max_bytes", self.text_clipboard_max_bytes),
            ("image_clipboard_max_bytes", self.image_clipboard_max_bytes),
        ] {
            if !(1..=256 * 1024 * 1024).contains(&value) {
                return Err(format!("{name} must be between 1 and 268435456"));
            }
        }
        if !(64 * 1024..=64 * 1024 * 1024).contains(&self.transfer_chunk_max_bytes) {
            return Err("transfer_chunk_max_bytes must be between 65536 and 67108864".into());
        }
        if !(1..=1_000_000).contains(&self.terminal_history_lines) {
            return Err("terminal_history_lines must be between 1 and 1000000".into());
        }
        for (name, path) in [
            ("runtime_dir", &self.runtime_dir),
            ("password_file", &self.password_file),
            ("endpoint_file", &self.endpoint_file),
        ] {
            if path
                .as_ref()
                .is_some_and(|p| !p.is_absolute() || p.to_str().is_none())
            {
                return Err(format!("{name} must be an absolute UTF-8 path"));
            }
        }
        if self.password_file.is_some() && self.password_file == self.endpoint_file {
            return Err("password_file and endpoint_file must be different paths".into());
        }
        Ok(())
    }

    pub fn runtime_dir(&self) -> Result<PathBuf, String> {
        let path = match self.runtime_dir.clone().or_else(|| {
            env::var_os("XDG_RUNTIME_DIR")
                .filter(|v| !v.is_empty())
                .map(PathBuf::from)
        }) {
            Some(path) => path,
            None => home_dir()?.join(".cache/omaviewer/runtime"),
        };
        if !path.is_absolute() || path.to_str().is_none() {
            return Err("runtime directory must be an absolute UTF-8 path".into());
        }
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&path)
            .map_err(|e| format!("cannot create runtime directory {}: {e}", path.display()))?;
        let meta = fs::symlink_metadata(&path).map_err(|e| e.to_string())?;
        if !meta.is_dir() || meta.uid() != unsafe { libc::geteuid() } {
            return Err(
                "runtime directory must be a directory owned by the current user, not a symlink"
                    .into(),
            );
        }
        if meta.mode() & 0o077 != 0 {
            return Err(format!(
                "runtime directory {} must be private (chmod 700)",
                path.display()
            ));
        }
        // Keep mode explicit for newly created directories regardless of umask.
        fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).map_err(|e| e.to_string())?;
        Ok(path)
    }
}

/// TOML basic string, including control characters that JSON escapes differently.
pub fn toml_string(value: &str) -> String {
    let mut result = String::from("\"");
    for c in value.chars() {
        match c {
            '"' => result.push_str("\\\""),
            '\\' => result.push_str("\\\\"),
            c if c.is_control() => result.push_str(&format!("\\u{:04X}", c as u32)),
            c => result.push(c),
        }
    }
    result.push('"');
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    fn options(items: &[&str]) -> Vec<String> {
        items.iter().map(|v| (*v).into()).collect()
    }
    #[test]
    fn defaults_and_unknown_fields() {
        let cfg: HostConfig = serde_json::from_str("{}").unwrap();
        cfg.validate().unwrap();
        assert_eq!(cfg.control_bind, None);
        assert_eq!(cfg.text_clipboard_max_bytes, 1048576);
        assert!(serde_json::from_str::<HostConfig>(r#"{"fpss":60}"#).is_err());
    }
    #[test]
    fn cli_overrides_json_and_exclusive_choices() {
        let cfg: HostConfig = serde_json::from_str(
            r#"{"tailscale":true,"window":"0x12","fps":60,"username":"alice"}"#,
        )
        .unwrap();
        let cfg = cfg
            .apply_options(&options(&[
                "--bind",
                "127.0.0.1:3390",
                "--output",
                "DP-1",
                "--fps",
                "24",
                "--control-bind",
                "127.0.0.1",
            ]))
            .unwrap();
        assert!(!cfg.tailscale);
        assert_eq!(cfg.window, None);
        assert_eq!(cfg.fps, 24);
        assert_eq!(cfg.username, "alice");
        assert!(HostConfig::default()
            .apply_options(&options(&["--tailscale", "--bind", "127.0.0.1:3389"]))
            .is_err());
    }
    #[test]
    fn rejects_invalid_settings() {
        for json in [
            r#"{"fps":0}"#,
            r#"{"control_port":3389}"#,
            r#"{"username":""}"#,
            r#"{"runtime_dir":"relative"}"#,
            r#"{"image_clipboard_max_bytes":0}"#,
            r#"{"transfer_chunk_max_bytes":1}"#,
            r#"{"transfer_chunk_max_bytes":67108865}"#,
            r#"{"terminal_history_lines":1000001}"#,
            r#"{"window":"not-an-address"}"#,
            r#"{"bind":"127.0.0.1:0"}"#,
        ] {
            assert!(
                serde_json::from_str::<HostConfig>(json)
                    .unwrap()
                    .validate()
                    .is_err(),
                "{json}"
            );
        }
    }
    #[test]
    fn escapes_backend_strings() {
        assert_eq!(
            toml_string("a\"\\\n\0\t雪"),
            "\"a\\\"\\\\\\u000A\\u0000\\u0009雪\""
        );
    }
    #[test]
    fn custom_limits_and_paths() {
        let cfg = HostConfig::default()
            .apply_options(&options(&[
                "--text-clipboard-max-bytes",
                "4096",
                "--image-clipboard-max-bytes",
                "8192",
                "--transfer-chunk-max-bytes",
                "65536",
                "--terminal-history-lines",
                "500",
                "--username",
                "someone",
                "--backend",
                "/opt/rdp/bin/hypr-rdp",
            ]))
            .unwrap();
        assert_eq!(cfg.text_clipboard_max_bytes, 4096);
        assert_eq!(cfg.image_clipboard_max_bytes, 8192);
        assert_eq!(cfg.transfer_chunk_max_bytes, 65536);
        assert_eq!(cfg.terminal_history_lines, 500);
        assert_eq!(cfg.backend, PathBuf::from("/opt/rdp/bin/hypr-rdp"));
    }
    #[test]
    fn loads_explicit_file_with_cli_precedence() {
        let path = env::temp_dir().join(format!("omaviewer-config-test-{}.json", std::process::id()));
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
            .unwrap();
        use std::io::Write;
        file.write_all(br#"{"fps":50,"username":"from-json","control_bind":"127.0.0.1"}"#)
            .unwrap();
        let args = vec![
            "--config".into(),
            path.to_str().unwrap().into(),
            "--fps".into(),
            "45".into(),
        ];
        let config = HostConfig::from_args(&args).unwrap();
        fs::remove_file(&path).unwrap();
        assert_eq!(config.fps, 45);
        assert_eq!(config.username, "from-json");
        assert_eq!(config.control_bind, Some("127.0.0.1".parse().unwrap()));
        assert!(HostConfig::from_args(&args)
            .unwrap_err()
            .contains("cannot read host config"));
    }
    #[test]
    fn runtime_directory_is_private_and_rejects_symlinks() {
        let base = env::temp_dir().join(format!("omaviewer-runtime-test-{}", std::process::id()));
        fs::DirBuilder::new().mode(0o700).create(&base).unwrap();
        let path = base.join("runtime");
        let cfg = HostConfig {
            runtime_dir: Some(path.clone()),
            ..HostConfig::default()
        };
        assert_eq!(cfg.runtime_dir().unwrap(), path);
        assert_eq!(fs::metadata(&path).unwrap().mode() & 0o777, 0o700);
        let link = base.join("link");
        std::os::unix::fs::symlink(&path, &link).unwrap();
        let cfg = HostConfig {
            runtime_dir: Some(link),
            ..HostConfig::default()
        };
        assert!(cfg.runtime_dir().is_err());
        fs::remove_dir_all(base).unwrap();
    }
}
