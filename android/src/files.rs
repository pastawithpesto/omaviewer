//! Authenticated access to the host's shared files folder.

use windowlink_shared::{MAX_FILE_BYTES, SharedFileInfo, encode_file_path_segment};

pub fn list(host: &str, port: u16, token: &str) -> Result<Vec<SharedFileInfo>, String> {
    let (status, body) =
        crate::window_picker::request(host, port, token, "GET", "/files", "", &[])?;
    if status != 200 {
        return Err(format!("Could not list shared files (HTTP {status})"));
    }
    let values: Vec<serde_json::Value> = serde_json::from_slice(&body)
        .map_err(|error| format!("Invalid file list from host: {error}"))?;
    values
        .iter()
        .map(|value| {
            let name = value
                .get("name")
                .and_then(|v| v.as_str())
                .ok_or_else(|| "File list entry has no name".to_owned())?;
            let size = value
                .get("size")
                .and_then(|v| v.as_u64())
                .ok_or_else(|| "File list entry has invalid size".to_owned())?;
            let modified_unix = value.get("modified_unix").and_then(|v| v.as_u64());
            Ok(SharedFileInfo {
                name: name.to_owned(),
                size,
                modified_unix,
            })
        })
        .collect()
}

pub fn upload(host: &str, port: u16, token: &str, name: &str, bytes: &[u8]) -> Result<(), String> {
    if bytes.len() as u64 > MAX_FILE_BYTES {
        return Err("File exceeds 100 MiB".into());
    }
    let path = file_path(name)?;
    let (status, _) = crate::window_picker::request(
        host,
        port,
        token,
        "POST",
        &path,
        "application/octet-stream",
        bytes,
    )?;
    if status == 200 || status == 201 {
        Ok(())
    } else {
        Err(format!("Upload failed (HTTP {status})"))
    }
}

pub fn download(host: &str, port: u16, token: &str, name: &str) -> Result<Vec<u8>, String> {
    let path = file_path(name)?;
    let response = crate::window_picker::request_with_limit(
        host,
        port,
        token,
        "GET",
        &path,
        "",
        &[],
        MAX_FILE_BYTES as usize,
        &[],
    )?;
    if response.status == 200 {
        Ok(response.body)
    } else {
        Err(format!("Download failed (HTTP {})", response.status))
    }
}

fn file_path(name: &str) -> Result<String, String> {
    encode_file_path_segment(name)
        .map(|segment| format!("/files/{segment}"))
        .ok_or_else(|| "Invalid filename".into())
}
