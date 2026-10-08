//! Typed clipboard snapshots with bounded image decoding and legacy text access.
use crate::control::{send_bytes, send_etag_response, ApiError, Request};
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::{
    io::{self, Cursor, Read, Write},
    net::TcpStream,
    os::unix::io::AsRawFd,
    process::{Child, Command, Stdio},
    sync::Mutex,
    thread,
    time::{Duration, Instant},
};

pub(crate) fn text_max() -> usize {
    crate::config::get().text_clipboard_max_bytes
}
pub(crate) fn image_max() -> usize {
    crate::config::get().image_clipboard_max_bytes
}
const COMMAND_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Default)]
pub(crate) struct ClipboardService {
    revision: Mutex<(String, u64)>,
}
#[derive(Serialize)]
struct Descriptor {
    mime: Option<String>,
    revision: u64,
    byte_length: usize,
    content_id: String,
}
struct Snapshot {
    descriptor: Descriptor,
    bytes: Vec<u8>,
}

impl ClipboardService {
    fn snapshot(&self) -> Result<Snapshot, ApiError> {
        let mut revision = self
            .revision
            .lock()
            .map_err(|_| ApiError::new(500, "clipboard state unavailable"))?;
        let types = read_command(&["--list-types"], 64 * 1024, true)?;
        let types = String::from_utf8_lossy(&types);
        let mime = if types.lines().any(|m| m == "image/png") {
            Some("image/png")
        } else if types.lines().any(|m| m == "image/jpeg") {
            Some("image/jpeg")
        } else if types
            .lines()
            .any(|m| m.starts_with("text/plain") || m == "UTF8_STRING")
        {
            Some("text/plain")
        } else {
            None
        };
        let (mime, bytes) = match mime {
            Some("image/png") => (
                Some("image/png".to_owned()),
                normalize_image(
                    "image/png",
                    read_command(&["--no-newline", "--type", "image/png"], image_max(), false)?,
                )?,
            ),
            Some("image/jpeg") => (
                Some("image/png".to_owned()),
                normalize_image(
                    "image/jpeg",
                    read_command(
                        &["--no-newline", "--type", "image/jpeg"],
                        image_max(),
                        false,
                    )?,
                )?,
            ),
            Some("text/plain") => (
                Some("text/plain; charset=utf-8".to_owned()),
                read_text()?.into_bytes(),
            ),
            _ => (None, Vec::new()),
        };
        let content_id = content_identity(mime.as_deref(), &bytes);
        if revision.0 != content_id {
            revision.0 = content_id.clone();
            revision.1 = revision.1.saturating_add(1);
        }
        Ok(Snapshot {
            descriptor: Descriptor {
                mime,
                revision: revision.1,
                byte_length: bytes.len(),
                content_id,
            },
            bytes,
        })
    }
    pub(crate) fn route(
        &self,
        request: &Request,
        stream: &mut TcpStream,
    ) -> Option<Result<(), ApiError>> {
        let path = request.path.split('?').next().unwrap_or("");
        if path != "/clipboard/descriptor" && path != "/clipboard/content" {
            return None;
        }
        Some((|| match (request.method.as_str(), path) {
            ("GET", _) => {
                let snapshot = self.snapshot()?;
                let etag = format!("\"{}\"", snapshot.descriptor.content_id);
                if let Some(expected) = request.header("if-match") {
                    if expected != etag {
                        return Err(ApiError::new(
                            412,
                            "clipboard changed; read its descriptor again",
                        ));
                    }
                }
                if request.header("if-none-match") == Some(etag.as_str()) {
                    return send_etag_response(stream, 304, "application/json", &[], &etag)
                        .map_err(|e| ApiError::new(500, e.to_string()));
                }
                if path == "/clipboard/descriptor" {
                    let body = serde_json::to_vec(&snapshot.descriptor)
                        .map_err(|e| ApiError::new(500, e.to_string()))?;
                    send_etag_response(stream, 200, "application/json", &body, &etag)
                        .map_err(|e| ApiError::new(500, e.to_string()))
                } else {
                    send_etag_response(
                        stream,
                        if snapshot.descriptor.mime.is_some() {
                            200
                        } else {
                            204
                        },
                        snapshot
                            .descriptor
                            .mime
                            .as_deref()
                            .unwrap_or("application/octet-stream"),
                        &snapshot.bytes,
                        &etag,
                    )
                    .map_err(|e| ApiError::new(500, e.to_string()))
                }
            }
            ("POST", "/clipboard/content") => {
                let mime = request
                    .header("content-type")
                    .unwrap_or("")
                    .split(';')
                    .next()
                    .unwrap_or("")
                    .trim()
                    .to_ascii_lowercase();
                let bytes = match mime.as_str() {
                    "text/plain" => {
                        if request.body.len() > text_max() {
                            return Err(ApiError::new(
                                413,
                                "clipboard text exceeds the configured limit",
                            ));
                        }
                        std::str::from_utf8(&request.body)
                            .map_err(|_| ApiError::new(400, "clipboard text must be UTF-8"))?;
                        request.body.clone()
                    }
                    "image/png" | "image/jpeg" => normalize_image(&mime, request.body.clone())?,
                    _ => {
                        return Err(ApiError::new(
                            415,
                            "clipboard accepts text/plain, image/png or image/jpeg",
                        ))
                    }
                };
                write_clipboard(
                    if mime == "text/plain" {
                        "text/plain"
                    } else {
                        "image/png"
                    },
                    &bytes,
                )?;
                send_bytes(stream, 204, "application/json", &[])
                    .map_err(|e| ApiError::new(500, e.to_string()))
            }
            _ => Err(ApiError::new(404, "unknown clipboard route")),
        })())
    }
}
fn content_identity(mime: Option<&str>, bytes: &[u8]) -> String {
    let mut hash = Sha256::new();
    hash.update(mime.unwrap_or("").as_bytes());
    hash.update([0]);
    hash.update(bytes);
    format!("{:x}", hash.finalize())
}
fn normalize_image(mime: &str, bytes: Vec<u8>) -> Result<Vec<u8>, ApiError> {
    if bytes.len() > image_max() {
        return Err(ApiError::new(
            413,
            "clipboard image exceeds the configured limit",
        ));
    }
    let format = match mime {
        "image/png" => image::ImageFormat::Png,
        "image/jpeg" => image::ImageFormat::Jpeg,
        _ => return Err(ApiError::new(415, "unsupported image MIME type")),
    };
    let mut reader = image::ImageReader::with_format(Cursor::new(&bytes), format);
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(8192);
    limits.max_image_height = Some(8192);
    limits.max_alloc = Some(64 * 1024 * 1024);
    reader.limits(limits);
    let decoded = reader
        .decode()
        .map_err(|e| ApiError::new(400, format!("invalid or oversized clipboard image: {e}")))?;
    if format == image::ImageFormat::Png {
        return Ok(bytes);
    }
    let mut png = Cursor::new(Vec::new());
    decoded
        .write_to(&mut png, image::ImageFormat::Png)
        .map_err(|e| ApiError::new(400, e.to_string()))?;
    let png = png.into_inner();
    if png.len() > image_max() {
        return Err(ApiError::new(
            413,
            "converted clipboard image exceeds the configured limit",
        ));
    }
    Ok(png)
}
fn nonblocking(fd: libc::c_int) -> Result<(), ApiError> {
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags < 0 || unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
        return Err(ApiError::new(500, io::Error::last_os_error().to_string()));
    }
    Ok(())
}
fn terminate(child: &mut Child) {
    let _ = child.kill();
    let _ = child.wait();
}
fn read_command(args: &[&str], limit: usize, allow_empty: bool) -> Result<Vec<u8>, ApiError> {
    let mut child = Command::new("wl-paste")
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| ApiError::new(500, format!("cannot run wl-paste: {e}")))?;
    let result = (|| {
        let mut stdout = child
            .stdout
            .take()
            .ok_or_else(|| ApiError::new(500, "wl-paste has no stdout"))?;
        nonblocking(stdout.as_raw_fd())?;
        let deadline = Instant::now() + COMMAND_TIMEOUT;
        let mut bytes = Vec::new();
        let mut buffer = [0; 8192];
        loop {
            if Instant::now() >= deadline {
                return Err(ApiError::new(504, "clipboard read timed out"));
            }
            match stdout.read(&mut buffer) {
                Ok(0) => {
                    if let Some(status) = child
                        .try_wait()
                        .map_err(|e| ApiError::new(500, e.to_string()))?
                    {
                        if !status.success() && !(allow_empty && bytes.is_empty()) {
                            return Err(ApiError::new(500, "wl-paste failed"));
                        }
                        return Ok(bytes);
                    }
                }
                Ok(n) => {
                    if n > limit.saturating_sub(bytes.len()) {
                        return Err(ApiError::new(
                            413,
                            "desktop clipboard exceeds its size limit",
                        ));
                    }
                    bytes.extend_from_slice(&buffer[..n]);
                    continue;
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {}
                Err(e) => return Err(ApiError::new(500, e.to_string())),
            }
            thread::sleep(Duration::from_millis(5));
        }
    })();
    if result.is_err() {
        terminate(&mut child);
    }
    result
}
pub(crate) fn read_text() -> Result<String, ApiError> {
    String::from_utf8(read_command(
        &["--no-newline", "--type", "text/plain"],
        text_max(),
        true,
    )?)
    .map_err(|_| ApiError::new(500, "desktop clipboard is not UTF-8"))
}
pub(crate) fn write_text(text: &str) -> Result<(), ApiError> {
    write_clipboard("text/plain", text.as_bytes())
}
fn write_clipboard(mime: &str, bytes: &[u8]) -> Result<(), ApiError> {
    let mut child = Command::new("wl-copy")
        .args(["--type", mime])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| ApiError::new(500, format!("cannot run wl-copy: {e}")))?;
    let result = (|| {
        let mut stdin = child
            .stdin
            .take()
            .ok_or_else(|| ApiError::new(500, "wl-copy has no stdin"))?;
        nonblocking(stdin.as_raw_fd())?;
        let deadline = Instant::now() + COMMAND_TIMEOUT;
        let mut offset = 0;
        while offset < bytes.len() {
            if Instant::now() >= deadline {
                return Err(ApiError::new(504, "clipboard write timed out"));
            }
            match stdin.write(&bytes[offset..]) {
                Ok(0) => return Err(ApiError::new(500, "clipboard write ended early")),
                Ok(n) => offset += n,
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                    thread::sleep(Duration::from_millis(5))
                }
                Err(e) => return Err(ApiError::new(500, e.to_string())),
            }
        }
        drop(stdin);
        loop {
            if let Some(status) = child
                .try_wait()
                .map_err(|e| ApiError::new(500, e.to_string()))?
            {
                return if status.success() {
                    Ok(())
                } else {
                    Err(ApiError::new(500, "wl-copy failed"))
                };
            }
            if Instant::now() >= deadline {
                return Err(ApiError::new(504, "clipboard write timed out"));
            }
            thread::sleep(Duration::from_millis(5));
        }
    })();
    if result.is_err() {
        terminate(&mut child);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn malformed_images_do_not_reach_clipboard() {
        assert!(normalize_image("image/png", b"not a png".to_vec()).is_err());
    }
    #[test]
    fn png_identity_survives_validation() {
        let mut png = Cursor::new(Vec::new());
        image::DynamicImage::new_rgba8(2, 2)
            .write_to(&mut png, image::ImageFormat::Png)
            .unwrap();
        let bytes = png.into_inner();
        assert_eq!(normalize_image("image/png", bytes.clone()).unwrap(), bytes);
    }
    #[test]
    fn identity_includes_mime() {
        assert_ne!(
            content_identity(Some("text/plain"), b"x"),
            content_identity(Some("image/png"), b"x")
        );
    }
}
