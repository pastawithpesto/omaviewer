use serde_json::{Value, json};
use std::path::PathBuf;

#[derive(Clone, Default, PartialEq)]
pub struct SavedConnection {
    pub name: String,
    pub host: String,
    pub password: String,
    pub username: String,
    pub port: String,
    pub control_host: String,
    pub control_port: String,
    pub last_view: String,
}

impl SavedConnection {
    fn from_json(value: &Value) -> Option<Self> {
        let host = value.get("host")?.as_str()?.trim().to_owned();
        if host.is_empty() {
            return None;
        }
        let string_field = |key: &str| {
            value
                .get(key)
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned()
        };
        Some(Self {
            name: {
                let name = string_field("name");
                if name.is_empty() { host.clone() } else { name }
            },
            host,
            password: string_field("password"),
            username: string_field("username"),
            port: string_field("port"),
            control_host: string_field("controlHost"),
            control_port: string_field("controlPort"),
            last_view: string_field("lastView"),
        })
    }

    pub fn label(&self) -> String {
        format!("{}  ·  {}:{}", self.name, self.host, self.port)
    }

    pub fn matches(&self, host: &str, port: &str) -> bool {
        self.host == host.trim() && self.port.trim_end() == port.trim()
    }

    fn to_json(&self) -> Value {
        json!({
            "name": self.name,
            "host": self.host,
            "password": self.password,
            "username": self.username,
            "port": self.port,
            "controlHost": self.control_host,
            "controlPort": self.control_port,
            "lastView": self.last_view,
        })
    }
}

fn file_name(path: &Option<PathBuf>) -> PathBuf {
    path.clone()
        .unwrap_or_else(|| PathBuf::from("/tmp"))
        .join("windowlink-connections.json")
}

pub fn load(path: &Option<PathBuf>) -> Vec<SavedConnection> {
    let Ok(text) = std::fs::read_to_string(file_name(path)) else {
        return Vec::new();
    };
    match serde_json::from_str::<Value>(&text) {
        Ok(Value::Array(items)) => items
            .iter()
            .filter_map(SavedConnection::from_json)
            .collect(),
        _ => Vec::new(),
    }
}

pub fn store(path: &Option<PathBuf>, connections: &[SavedConnection]) -> Result<(), String> {
    let text = serde_json::to_string_pretty(
        &connections
            .iter()
            .map(SavedConnection::to_json)
            .collect::<Vec<_>>(),
    )
    .map_err(|error| error.to_string())?;
    use std::io::Write;
    #[cfg(unix)]
    use std::os::unix::fs::OpenOptionsExt;
    let destination = file_name(path);
    let temporary = destination.with_extension("json.tmp");
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    options.mode(0o600);
    let mut file = options
        .open(&temporary)
        .map_err(|error| error.to_string())?;
    file.write_all(text.as_bytes())
        .and_then(|()| file.sync_all())
        .map_err(|error| error.to_string())?;
    std::fs::rename(temporary, destination).map_err(|error| error.to_string())
}

/// Insert or replace the entry matching `conn` host/port, keeping newest position.
pub fn upsert(connections: &mut Vec<SavedConnection>, conn: SavedConnection) {
    connections.retain(|existing| !existing.matches(&conn.host, &conn.port));
    connections.insert(0, conn);
}

pub fn remove(connections: &mut Vec<SavedConnection>, index: usize) {
    if index < connections.len() {
        connections.remove(index);
    }
}

pub fn load_preview_interval(path: &Option<PathBuf>) -> u64 {
    let file = file_name(path).with_file_name("windowlink-settings.json");
    let value = std::fs::read(file)
        .ok()
        .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
        .and_then(|value| value.get("preview_interval_secs").and_then(Value::as_u64));
    match value {
        Some(0) => 0,
        Some(seconds) => seconds.clamp(5, 300),
        None => 30,
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ControlAlignment {
    Left,
    #[default]
    Center,
    Right,
}
impl ControlAlignment {
    pub fn label(self) -> &'static str {
        match self {
            Self::Left => "Left",
            Self::Center => "Center",
            Self::Right => "Right",
        }
    }
}
pub fn load_control_alignment(path: &Option<PathBuf>) -> ControlAlignment {
    let value = load_settings(path);
    match value["terminal_control_alignment"].as_str() {
        Some("Left") => ControlAlignment::Left,
        Some("Right") => ControlAlignment::Right,
        _ => ControlAlignment::Center,
    }
}
pub fn store_control_alignment(
    path: &Option<PathBuf>,
    alignment: ControlAlignment,
) -> Result<(), String> {
    store_setting(path, "terminal_control_alignment", json!(alignment.label()))
}
fn load_settings(path: &Option<PathBuf>) -> Value {
    std::fs::read(file_name(path).with_file_name("windowlink-settings.json"))
        .ok()
        .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
        .filter(Value::is_object)
        .unwrap_or_else(|| json!({}))
}
#[derive(Clone, Debug, PartialEq)]
pub struct AppSettings {
    pub terminal_font_size: u8,
    pub control_alignment: ControlAlignment,
    pub preview_interval_secs: u64,
    pub text_clipboard_sync: bool,
    pub image_clipboard_sync: bool,
    pub staging_cache_max_mib: u32,
    pub theme: String,
}
impl Default for AppSettings {
    fn default() -> Self {
        Self {
            terminal_font_size: 18,
            control_alignment: ControlAlignment::Center,
            preview_interval_secs: 30,
            text_clipboard_sync: true,
            image_clipboard_sync: true,
            staging_cache_max_mib: 1024,
            theme: "Tokyo Night".into(),
        }
    }
}
pub fn load_preferences(path: &Option<PathBuf>) -> AppSettings {
    let value = load_settings(path);
    let defaults = AppSettings::default();
    let alignment = match value["terminal_control_alignment"].as_str() {
        Some("Left") => ControlAlignment::Left,
        Some("Right") => ControlAlignment::Right,
        _ => ControlAlignment::Center,
    };
    let interval = value["preview_interval_secs"]
        .as_u64()
        .map(|seconds| {
            if seconds == 0 {
                0
            } else {
                seconds.clamp(5, 300)
            }
        })
        .unwrap_or(defaults.preview_interval_secs);
    let theme = value["theme"]
        .as_str()
        .filter(|theme| matches!(*theme, "Catppuccin" | "Gruvbox" | "Rosé Pine"))
        .unwrap_or(&defaults.theme)
        .to_owned();
    AppSettings {
        terminal_font_size: value["terminal_font_size"]
            .as_u64()
            .unwrap_or(18)
            .clamp(10, 28) as u8,
        control_alignment: alignment,
        preview_interval_secs: interval,
        text_clipboard_sync: value["text_clipboard_sync"].as_bool().unwrap_or(true),
        image_clipboard_sync: value["image_clipboard_sync"].as_bool().unwrap_or(true),
        staging_cache_max_mib: value["staging_cache_max_mib"]
            .as_u64()
            .unwrap_or(1024)
            .clamp(64, 4096) as u32,
        theme,
    }
}
pub fn store_preferences(path: &Option<PathBuf>, settings: &AppSettings) -> Result<(), String> {
    let file = file_name(path).with_file_name("windowlink-settings.json");
    let temporary = file.with_extension("json.tmp");
    let mut value = load_settings(path);
    value["terminal_font_size"] = json!(settings.terminal_font_size.clamp(10, 28));
    value["terminal_control_alignment"] = json!(settings.control_alignment.label());
    value["preview_interval_secs"] = json!(if settings.preview_interval_secs == 0 {
        0
    } else {
        settings.preview_interval_secs.clamp(5, 300)
    });
    value["text_clipboard_sync"] = json!(settings.text_clipboard_sync);
    value["image_clipboard_sync"] = json!(settings.image_clipboard_sync);
    value["staging_cache_max_mib"] = json!(settings.staging_cache_max_mib.clamp(64, 4096));
    value["theme"] = json!(match settings.theme.as_str() {
        "Catppuccin" | "Gruvbox" | "Rosé Pine" => settings.theme.as_str(),
        _ => "Tokyo Night",
    });
    std::fs::write(&temporary, value.to_string())
        .and_then(|()| std::fs::rename(temporary, file))
        .map_err(|error| error.to_string())
}
fn store_setting(path: &Option<PathBuf>, key: &str, value: Value) -> Result<(), String> {
    let file = file_name(path).with_file_name("windowlink-settings.json");
    let temporary = file.with_extension("json.tmp");
    let mut settings = load_settings(path);
    settings[key] = value;
    std::fs::write(&temporary, settings.to_string())
        .and_then(|()| std::fs::rename(temporary, file))
        .map_err(|error| error.to_string())
}
pub fn store_preview_interval(path: &Option<PathBuf>, seconds: u64) -> Result<(), String> {
    store_setting(
        path,
        "preview_interval_secs",
        json!(if seconds == 0 {
            0
        } else {
            seconds.clamp(5, 300)
        }),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn alignment_defaults_to_center_and_settings_preserve_each_other() {
        let directory =
            std::env::temp_dir().join(format!("windowlink-alignment-test-{}", std::process::id()));
        std::fs::create_dir_all(&directory).unwrap();
        let path = Some(directory.clone());
        assert_eq!(load_control_alignment(&path), ControlAlignment::Center);
        store_preview_interval(&path, 60).unwrap();
        for alignment in [
            ControlAlignment::Left,
            ControlAlignment::Right,
            ControlAlignment::Center,
        ] {
            store_control_alignment(&path, alignment).unwrap();
            assert_eq!(load_control_alignment(&path), alignment);
            assert_eq!(load_preview_interval(&path), 60);
        }
        store_control_alignment(&path, ControlAlignment::Right).unwrap();
        store_preview_interval(&path, 5).unwrap();
        assert_eq!(load_control_alignment(&path), ControlAlignment::Right);
        std::fs::remove_file(directory.join("windowlink-settings.json")).unwrap();
        std::fs::remove_dir(directory).unwrap();
    }
    #[test]
    fn app_preferences_round_trip_with_safe_defaults_and_bounds() {
        let directory = std::env::temp_dir().join(format!(
            "windowlink-preferences-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&directory).unwrap();
        let path = Some(directory.clone());
        std::fs::write(
            directory.join("windowlink-settings.json"),
            r#"{"another_component":"keep me"}"#,
        )
        .unwrap();
        assert_eq!(load_preferences(&path), AppSettings::default());
        let settings = AppSettings {
            terminal_font_size: 23,
            control_alignment: ControlAlignment::Right,
            preview_interval_secs: 60,
            text_clipboard_sync: false,
            image_clipboard_sync: true,
            staging_cache_max_mib: 2048,
            theme: "Gruvbox".into(),
        };
        store_preferences(&path, &settings).unwrap();
        assert_eq!(load_preferences(&path), settings);
        let saved: Value = serde_json::from_slice(
            &std::fs::read(directory.join("windowlink-settings.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(saved["another_component"], "keep me");
        let mut bounded = settings;
        bounded.terminal_font_size = 2;
        bounded.preview_interval_secs = 900;
        bounded.staging_cache_max_mib = 8192;
        store_preferences(&path, &bounded).unwrap();
        let restored = load_preferences(&path);
        assert_eq!(restored.terminal_font_size, 10);
        assert_eq!(restored.preview_interval_secs, 300);
        assert_eq!(restored.staging_cache_max_mib, 4096);
        std::fs::remove_file(directory.join("windowlink-settings.json")).unwrap();
        std::fs::remove_dir(directory).unwrap();
    }
    #[test]
    fn profiles_keep_separate_credentials_and_last_views() {
        let first = SavedConnection {
            name: "Desktop A".into(),
            host: "100.64.0.1".into(),
            port: "3389".into(),
            password: "fixture-a".into(),
            last_view: "terminal".into(),
            ..Default::default()
        };
        let second = SavedConnection {
            host: "100.64.0.2".into(),
            password: "fixture-b".into(),
            last_view: "files".into(),
            ..first.clone()
        };
        let mut profiles = vec![];
        upsert(&mut profiles, first.clone());
        upsert(&mut profiles, second.clone());
        assert_eq!(profiles.len(), 2);
        for profile in &profiles {
            let restored = SavedConnection::from_json(&profile.to_json()).unwrap();
            assert!(restored == *profile);
        }
        let mut renamed = first;
        renamed.name = "Home".into();
        upsert(&mut profiles, renamed);
        assert_eq!(profiles.len(), 2);
        assert_eq!(profiles[0].name, "Home");
        assert!(profiles[1] == second);
    }
}
