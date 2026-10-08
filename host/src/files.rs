//! Filesystem operations run with this process's desktop-user credentials.
//! Directory handles anchor mutations even if a directory is moved meanwhile.
use crate::control::{send_bytes, ApiError, Request};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::{
    collections::HashMap,
    ffi::{OsStr, OsString},
    fs,
    io::{self, Cursor, Read, Seek, SeekFrom, Write},
    net::TcpStream,
    os::unix::{
        ffi::{OsStrExt, OsStringExt},
        fs::{MetadataExt, OpenOptionsExt, PermissionsExt},
        io::AsRawFd,
    },
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc, Mutex,
    },
    time::{Duration, Instant, UNIX_EPOCH},
};

const BUFFER: usize = 64 * 1024;
const MAX_TRANSFERS: usize = 32;
static NEXT_TRANSFER: AtomicU64 = AtomicU64::new(0);
const RETENTION: Duration = Duration::from_secs(24 * 60 * 60);

pub(crate) fn io_error(error: io::Error) -> ApiError {
    let status = match error.kind() {
        io::ErrorKind::NotFound => 404,
        io::ErrorKind::PermissionDenied => 403,
        io::ErrorKind::AlreadyExists => 409,
        _ => match error.raw_os_error() {
            Some(libc::ENOSPC | libc::EDQUOT) => 507,
            Some(libc::EROFS) => 403,
            _ => 500,
        },
    };
    ApiError::new(status, error.to_string())
}

pub(crate) fn query(path: &str, key: &str) -> Result<Option<String>, ApiError> {
    for field in path
        .split_once('?')
        .map(|(_, q)| q)
        .unwrap_or("")
        .split('&')
    {
        if let Some((name, value)) = field.split_once('=') {
            if name == key {
                return String::from_utf8(percent_decode(value)?)
                    .map(Some)
                    .map_err(|_| ApiError::new(400, "query value must be UTF-8"));
            }
        }
    }
    Ok(None)
}

fn percent_decode(value: &str) -> Result<Vec<u8>, ApiError> {
    let bytes = value.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            let hex = bytes
                .get(i + 1..i + 3)
                .ok_or_else(|| ApiError::new(400, "malformed percent encoding"))?;
            let hex = std::str::from_utf8(hex)
                .map_err(|_| ApiError::new(400, "malformed percent encoding"))?;
            out.push(
                u8::from_str_radix(hex, 16)
                    .map_err(|_| ApiError::new(400, "malformed percent encoding"))?,
            );
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    Ok(out)
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn decode_id(id: &str) -> Result<PathBuf, ApiError> {
    if id.len() % 2 != 0 || !id.is_ascii() || id.len() > 8192 {
        return Err(ApiError::new(400, "invalid path_id"));
    }
    let bytes = (0..id.len())
        .step_by(2)
        .map(|i| {
            u8::from_str_radix(&id[i..i + 2], 16).map_err(|_| ApiError::new(400, "invalid path_id"))
        })
        .collect::<Result<Vec<_>, _>>()?;
    valid_path(PathBuf::from(OsString::from_vec(bytes)))
}

fn valid_path(path: PathBuf) -> Result<PathBuf, ApiError> {
    if !path.is_absolute() || path.as_os_str().as_bytes().contains(&0) {
        return Err(ApiError::new(
            400,
            "path must be absolute and contain no NUL",
        ));
    }
    Ok(path)
}

fn requested_path(request: &Request) -> Result<PathBuf, ApiError> {
    if let Some(id) = query(&request.path, "path_id")? {
        return decode_id(&id);
    }
    let path = match query(&request.path, "path")? {
        Some(path) => PathBuf::from(path),
        None => crate::config::home_dir().map_err(|e| ApiError::new(500, e))?,
    };
    valid_path(path)
}

fn anchored(directory: &fs::File, name: &OsStr) -> PathBuf {
    PathBuf::from(format!("/proc/self/fd/{}", directory.as_raw_fd())).join(name)
}

fn open_directory(path: &Path) -> Result<fs::File, ApiError> {
    match fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_CLOEXEC)
        .open(path)
    {
        Ok(file) => Ok(file),
        Err(error) if error.kind() == io::ErrorKind::PermissionDenied => fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_PATH | libc::O_DIRECTORY | libc::O_CLOEXEC)
            .open(path)
            .map_err(io_error),
        Err(error) => Err(io_error(error)),
    }
}

fn open_regular(path: &Path) -> Result<fs::File, ApiError> {
    // O_NONBLOCK prevents opening a FIFO from hanging before fstat rejects it.
    let file = fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK | libc::O_CLOEXEC)
        .open(path)
        .map_err(io_error)?;
    if !file.metadata().map_err(io_error)?.is_file() {
        return Err(ApiError::new(415, "only regular files can be transferred"));
    }
    Ok(file)
}

fn identity(meta: &fs::Metadata) -> String {
    format!(
        "{}:{}:{}:{}:{}:{}:{}:{}",
        meta.dev(),
        meta.ino(),
        meta.len(),
        meta.mtime(),
        meta.mtime_nsec(),
        meta.ctime(),
        meta.ctime_nsec(),
        meta.mode()
    )
}

#[derive(Serialize)]
struct Entry {
    name: String,
    path: Option<String>,
    path_id: String,
    kind: &'static str,
    size: u64,
    modified_unix: Option<u64>,
    #[serde(skip)]
    modified_sort: (i64, i64),
    source_id: String,
    symlink_target: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum SortKey {
    Name,
    Size,
    Modified,
    Type,
}

fn listing_order(request_path: &str) -> Result<Option<(SortKey, bool)>, ApiError> {
    let descending = match query(request_path, "descending")?.as_deref() {
        None | Some("false") => false,
        Some("true") => true,
        _ => return Err(ApiError::new(400, "descending must be true or false")),
    };
    let key = match query(request_path, "sort")?.as_deref() {
        None => return Ok(None),
        Some("name") => SortKey::Name,
        Some("size") => SortKey::Size,
        Some("modified") => SortKey::Modified,
        Some("type") => SortKey::Type,
        _ => {
            return Err(ApiError::new(
                400,
                "sort must be name, size, modified, or type",
            ))
        }
    };
    Ok(Some((key, descending)))
}

// Omitting sort preserves native directory order for existing folder workers.
// Explicit sorting covers the entire directory before pagination, with no
// directories-first grouping. Equal keys always use ascending raw path bytes.
fn list_directory(
    path: &Path,
    directory: &fs::File,
    offset: usize,
    limit: usize,
    order: Option<(SortKey, bool)>,
) -> Result<(Vec<Entry>, Option<usize>), ApiError> {
    let entries =
        fs::read_dir(format!("/proc/self/fd/{}", directory.as_raw_fd())).map_err(io_error)?;
    let mut out = Vec::new();
    let mut seen = 0;
    let mut more = false;
    for item in entries {
        let item = item.map_err(io_error)?;
        let name = item.file_name();
        if name.as_bytes().starts_with(b".windowlink-upload-") {
            continue;
        }
        if order.is_none() {
            if seen < offset {
                seen += 1;
                continue;
            }
            if out.len() == limit {
                more = true;
                break;
            }
        }
        let meta = fs::symlink_metadata(anchored(directory, &name)).map_err(io_error)?;
        let item_path = path.join(&name);
        let kind = if meta.is_dir() {
            "directory"
        } else if meta.is_file() {
            "file"
        } else if meta.file_type().is_symlink() {
            "symlink"
        } else {
            "special"
        };
        out.push(Entry {
            name: name.to_string_lossy().into_owned(),
            path: item_path.to_str().map(str::to_owned),
            path_id: hex(item_path.as_os_str().as_bytes()),
            kind,
            size: meta.len(),
            modified_unix: meta
                .modified()
                .ok()
                .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                .map(|d| d.as_secs()),
            modified_sort: (meta.mtime(), meta.mtime_nsec()),
            source_id: identity(&meta),
            symlink_target: if kind == "symlink" {
                fs::read_link(anchored(directory, &name))
                    .ok()
                    .map(|p| p.to_string_lossy().into_owned())
            } else {
                None
            },
        });
        seen += 1;
    }
    if let Some((key, descending)) = order {
        out.sort_by(|a, b| {
            let comparison = match key {
                SortKey::Name => a.name.cmp(&b.name),
                SortKey::Size => a.size.cmp(&b.size),
                SortKey::Modified => a.modified_sort.cmp(&b.modified_sort),
                SortKey::Type => a.kind.cmp(b.kind),
            };
            let comparison = if descending {
                comparison.reverse()
            } else {
                comparison
            };
            // Hex encoding preserves the ordering of the original path bytes.
            comparison.then_with(|| a.path_id.cmp(&b.path_id))
        });
        let end = offset.saturating_add(limit).min(out.len());
        let next = (end < out.len()).then_some(end);
        let page = out.into_iter().skip(offset).take(limit).collect();
        Ok((page, next))
    } else {
        Ok((out, more.then_some(seen)))
    }
}

fn json_response(
    stream: &mut TcpStream,
    status: u16,
    value: serde_json::Value,
) -> Result<(), ApiError> {
    send_bytes(
        stream,
        status,
        "application/json",
        value.to_string().as_bytes(),
    )
    .map_err(io_error)
}

pub(crate) fn fs_route(request: &Request, stream: &mut TcpStream) -> Option<Result<(), ApiError>> {
    let route = request.path.split('?').next().unwrap_or("");
    if !route.starts_with("/fs/") {
        return None;
    }
    Some((|| {
        if unsafe { libc::geteuid() } == 0 {
            return Err(ApiError::new(
                403,
                "filesystem access requires the desktop user, not root",
            ));
        }
        let path = requested_path(request)?;
        match (request.method.as_str(), route) {
            ("GET", "/fs/roots") => json_response(
                stream,
                200,
                json!({"home": crate::config::home_dir().map_err(|e| ApiError::new(500, e))?, "root":"/", "mounts": mounts(), "limits": {"transfer_chunk_max_bytes": crate::config::get().transfer_chunk_max_bytes}}),
            ),
            ("GET", "/fs/list") => {
                let directory = open_directory(&path)?;
                let directory_id = identity(&directory.metadata().map_err(io_error)?);
                if let Some(expected) = query(&request.path, "directory_id")? {
                    if expected != directory_id {
                        return Err(ApiError::new(409, "directory changed; refresh listing"));
                    }
                }
                let offset: usize = query(&request.path, "cursor")?
                    .unwrap_or_else(|| "0".into())
                    .parse()
                    .map_err(|_| ApiError::new(400, "invalid cursor"))?;
                let limit: usize = query(&request.path, "limit")?
                    .unwrap_or_else(|| "200".into())
                    .parse::<usize>()
                    .map_err(|_| ApiError::new(400, "invalid limit"))?
                    .clamp(1, 500);
                let order = listing_order(&request.path)?;
                let (out, next_cursor) = list_directory(&path, &directory, offset, limit, order)?;
                json_response(
                    stream,
                    200,
                    json!({"path":path.to_str(), "path_id":hex(path.as_os_str().as_bytes()), "parent":path.parent().and_then(Path::to_str), "parent_id":path.parent().map(|p|hex(p.as_os_str().as_bytes())), "entries":out, "directory_id":directory_id, "next_cursor":next_cursor}),
                )
            }
            ("POST", "/fs/mkdir") => {
                let spec: PathSpec = serde_json::from_slice(&request.body)
                    .map_err(|e| ApiError::new(400, e.to_string()))?;
                let path = spec.resolve()?;
                let (parent, name) = split_destination(&path)?;
                let directory = open_directory(parent)?;
                fs::create_dir(anchored(&directory, name)).map_err(io_error)?;
                json_response(
                    stream,
                    201,
                    json!({"path":path.to_str(),"path_id":hex(path.as_os_str().as_bytes())}),
                )
            }
            ("DELETE", "/fs/entry") => {
                let (parent, name) = split_destination(&path)?;
                let directory = open_directory(parent)?;
                let entry = anchored(&directory, name);
                let meta = fs::symlink_metadata(&entry).map_err(io_error)?;
                if let Some(expected) = query(&request.path, "source_id")? {
                    if expected != identity(&meta) {
                        return Err(ApiError::new(409, "entry changed"));
                    }
                }
                if meta.is_dir() {
                    fs::remove_dir(&entry)
                } else if meta.is_file() || meta.file_type().is_symlink() {
                    fs::remove_file(&entry)
                } else {
                    return Err(ApiError::new(
                        415,
                        "special files cannot be removed through transfers",
                    ));
                }
                .map_err(io_error)?;
                json_response(stream, 200, json!({"ok":true}))
            }
            _ => Err(ApiError::new(404, "unknown filesystem route")),
        }
    })())
}

fn mounts() -> Vec<String> {
    fs::read_to_string("/proc/self/mountinfo")
        .unwrap_or_default()
        .lines()
        .filter_map(|line| line.split_whitespace().nth(4))
        .map(|path| {
            path.replace("\\040", " ")
                .replace("\\011", "\t")
                .replace("\\134", "\\")
        })
        .collect()
}

#[derive(Deserialize)]
struct PathSpec {
    path: Option<String>,
    path_id: Option<String>,
}
impl PathSpec {
    fn resolve(&self) -> Result<PathBuf, ApiError> {
        if let Some(id) = &self.path_id {
            decode_id(id)
        } else {
            valid_path(PathBuf::from(self.path.as_ref().ok_or_else(|| {
                ApiError::new(400, "path or path_id is required")
            })?))
        }
    }
}
fn split_destination(path: &Path) -> Result<(&Path, &OsStr), ApiError> {
    let name = path
        .file_name()
        .ok_or_else(|| ApiError::new(400, "destination requires a filename"))?;
    let parent = path
        .parent()
        .ok_or_else(|| ApiError::new(400, "destination requires a parent"))?;
    Ok((parent, name))
}

#[derive(Deserialize)]
struct Start {
    #[serde(flatten)]
    destination: PathSpec,
    direction: String,
    size: Option<u64>,
    source_id: Option<String>,
    #[serde(default = "replace_policy")]
    conflict: String,
}
fn replace_policy() -> String {
    "skip".into()
}

struct Upload {
    directory: fs::File,
    staging_directory: Option<fs::File>,
    inplace: Option<fs::File>,
    temporary: OsString,
    final_name: OsString,
    original: Option<String>,
    replace: bool,
    offset: u64,
    partial_identity: String,
}
impl Upload {
    fn stage_directory(&self) -> &fs::File {
        self.staging_directory.as_ref().unwrap_or(&self.directory)
    }
}
struct TransferInner {
    file: fs::File,
    path: PathBuf,
    size: u64,
    source_id: String,
    state: &'static str,
    upload: Option<Upload>,
    touched: Instant,
    atomic: bool,
}
struct Transfer {
    cancelled: AtomicBool,
    inner: Mutex<TransferInner>,
}
impl Drop for TransferInner {
    fn drop(&mut self) {
        if let Some(upload) = &self.upload {
            let _ = fs::remove_file(anchored(upload.stage_directory(), &upload.temporary));
        }
    }
}
#[derive(Default)]
pub(crate) struct TransferManager {
    transfers: Mutex<HashMap<String, Arc<Transfer>>>,
}

impl TransferManager {
    fn get(&self, id: &str) -> Result<Arc<Transfer>, ApiError> {
        self.transfers
            .lock()
            .map_err(|_| ApiError::new(500, "transfer state unavailable"))?
            .get(id)
            .cloned()
            .ok_or_else(|| ApiError::new(404, "transfer not found"))
    }
    fn create(&self, spec: Start) -> Result<(String, Arc<Transfer>), ApiError> {
        // An elevated server would grant broader access than the desktop user.
        if unsafe { libc::geteuid() } == 0 {
            return Err(ApiError::new(
                403,
                "filesystem transfers require the desktop user, not root",
            ));
        }
        let mut transfers = self
            .transfers
            .lock()
            .map_err(|_| ApiError::new(500, "transfer state unavailable"))?;
        transfers.retain(|_, t| {
            t.inner
                .try_lock()
                .map(|s| s.touched.elapsed() < RETENTION && s.state != "complete")
                .unwrap_or(true)
        });
        if transfers.len() >= MAX_TRANSFERS {
            return Err(ApiError::new(
                429,
                "too many retained transfers; cancel an old transfer",
            ));
        }
        let path = spec.destination.resolve()?;
        let id = format!(
            "{:x}-{:x}",
            std::process::id(),
            NEXT_TRANSFER.fetch_add(1, Ordering::Relaxed)
        );
        let inner = match spec.direction.as_str() {
            "download" => {
                let file = open_regular(&path)?;
                let meta = file.metadata().map_err(io_error)?;
                TransferInner {
                    file,
                    path,
                    size: meta.len(),
                    source_id: identity(&meta),
                    state: "ready",
                    upload: None,
                    atomic: true,
                    touched: Instant::now(),
                }
            }
            "upload" => {
                let size = spec
                    .size
                    .ok_or_else(|| ApiError::new(400, "upload size is required"))?;
                let source_id = spec
                    .source_id
                    .filter(|s| !s.is_empty() && s.len() <= 512)
                    .ok_or_else(|| ApiError::new(400, "upload source_id is required"))?;
                let (parent, name) = split_destination(&path)?;
                let directory = open_directory(parent)?;
                let mut final_name = name.to_owned();
                let existing = match fs::symlink_metadata(anchored(&directory, name)) {
                    Ok(m) => Some(m),
                    Err(e) if e.kind() == io::ErrorKind::NotFound => None,
                    Err(e) => return Err(io_error(e)),
                };
                if existing.as_ref().is_some_and(|m| !m.is_file()) {
                    return Err(ApiError::new(
                        409,
                        "destination is not a regular file; resolve the link explicitly",
                    ));
                }
                let original = existing.as_ref().map(identity);
                match spec.conflict.as_str() {
                    "skip" if existing.is_some() => {
                        return Err(ApiError::new(
                            409,
                            "destination exists; choose replace or keep_both",
                        ))
                    }
                    "keep_both" if existing.is_some() => {
                        for n in 1..=10000 {
                            let mut candidate = name.to_os_string();
                            candidate.push(format!(" ({n})"));
                            match fs::symlink_metadata(anchored(&directory, &candidate)) {
                                Err(e) if e.kind() == io::ErrorKind::NotFound => {
                                    final_name = candidate;
                                    break;
                                }
                                Err(e) => return Err(io_error(e)),
                                _ => {}
                            }
                            if n == 10000 {
                                return Err(ApiError::new(409, "no unused destination name"));
                            }
                        }
                    }
                    "replace" | "replace_in_place" | "keep_both" | "skip" => {}
                    _ => {
                        return Err(ApiError::new(
                            400,
                            "conflict must be replace, replace_in_place, keep_both or skip",
                        ))
                    }
                }
                let temporary = OsString::from(format!(".windowlink-upload-{id}"));
                let (staging_directory, inplace) = if spec.conflict == "replace_in_place" {
                    if existing.is_none() {
                        return Err(ApiError::new(
                            409,
                            "in-place replacement requires an existing regular file",
                        ));
                    }
                    let writable = fs::OpenOptions::new()
                        .write(true)
                        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC)
                        .open(anchored(&directory, name))
                        .map_err(io_error)?;
                    let meta = writable.metadata().map_err(io_error)?;
                    if !meta.is_file() || Some(identity(&meta)) != original {
                        return Err(ApiError::new(409, "destination changed before opening it"));
                    }
                    let base = crate::config::get()
                        .runtime_dir()
                        .map_err(|e| ApiError::new(500, e))?;
                    let staging = base.join(format!("windowlink-transfers-{}", unsafe {
                        libc::geteuid()
                    }));
                    match fs::DirBuilder::new().create(&staging) {
                        Ok(()) => fs::set_permissions(&staging, fs::Permissions::from_mode(0o700))
                            .map_err(io_error)?,
                        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
                        Err(e) => return Err(io_error(e)),
                    }
                    let staging_directory = open_directory(&staging)?;
                    let meta = staging_directory.metadata().map_err(io_error)?;
                    if meta.uid() != unsafe { libc::geteuid() } || meta.mode() & 0o077 != 0 {
                        return Err(ApiError::new(
                            403,
                            "transfer staging directory must be private to the desktop user",
                        ));
                    }
                    (Some(staging_directory), Some(writable))
                } else {
                    (None, None)
                };
                let staging = staging_directory.as_ref().unwrap_or(&directory);
                let file=fs::OpenOptions::new().read(true).write(true).create_new(true).mode(if inplace.is_some(){0o600}else{0o666}).custom_flags(libc::O_NOFOLLOW|libc::O_CLOEXEC).open(anchored(staging,&temporary)).map_err(|e| {if e.kind()==io::ErrorKind::PermissionDenied {ApiError::new(403,"cannot stage beside destination: atomic upload requires a writable destination directory; existing writable files can use explicit replace_in_place")}else{io_error(e)}})?;
                if let Some(meta) = &existing {
                    if spec.conflict == "replace" {
                        file.set_permissions(fs::Permissions::from_mode(meta.mode() & 0o777))
                            .map_err(io_error)?;
                    }
                }
                let final_path = parent.join(&final_name);
                let partial_identity = identity(&file.metadata().map_err(io_error)?);
                TransferInner {
                    file,
                    path: final_path,
                    size,
                    source_id,
                    state: "ready",
                    atomic: inplace.is_none(),
                    upload: Some(Upload {
                        directory,
                        staging_directory,
                        inplace,
                        temporary,
                        final_name,
                        original,
                        replace: matches!(spec.conflict.as_str(), "replace" | "replace_in_place")
                            && existing.is_some(),
                        offset: 0,
                        partial_identity,
                    }),
                    touched: Instant::now(),
                }
            }
            _ => return Err(ApiError::new(400, "direction must be upload or download")),
        };
        let transfer = Arc::new(Transfer {
            cancelled: AtomicBool::new(false),
            inner: Mutex::new(inner),
        });
        transfers.insert(id.clone(), transfer.clone());
        Ok((id, transfer))
    }
    pub(crate) fn route(
        &self,
        request: &Request,
        stream: &mut TcpStream,
        stopping: &AtomicBool,
    ) -> Option<Result<(), ApiError>> {
        let route = request.path.split('?').next().unwrap_or("");
        if route != "/transfers" && !route.starts_with("/transfers/") {
            return None;
        }
        Some((|| {
            if route == "/transfers" && request.method == "POST" {
                let spec = serde_json::from_slice(&request.body)
                    .map_err(|e| ApiError::new(400, format!("invalid transfer request: {e}")))?;
                let (id, transfer) = self.create(spec)?;
                let state = transfer
                    .inner
                    .lock()
                    .map_err(|_| ApiError::new(500, "transfer unavailable"))?;
                return json_response(stream, 201, describe(&id, &state));
            }
            let tail = route
                .strip_prefix("/transfers/")
                .ok_or_else(|| ApiError::new(404, "unknown transfer route"))?;
            let (id, operation) = tail.split_once('/').unwrap_or((tail, ""));
            let transfer = self.get(id)?;
            if request.method == "DELETE" && operation.is_empty() {
                transfer.cancelled.store(true, Ordering::SeqCst);
                let retain = query(&request.path, "retain")?.as_deref() == Some("true");
                let mut state = transfer
                    .inner
                    .lock()
                    .map_err(|_| ApiError::new(500, "transfer unavailable"))?;
                state.state = if retain { "paused" } else { "cancelled" };
                state.touched = Instant::now();
                if !retain {
                    self.transfers
                        .lock()
                        .map_err(|_| ApiError::new(500, "transfer state unavailable"))?
                        .remove(id);
                    if let Some(upload) = state.upload.take() {
                        fs::remove_file(anchored(upload.stage_directory(), &upload.temporary))
                            .map_err(io_error)?;
                    }
                }
                return json_response(stream, 200, describe(id, &state));
            }
            let mut state = transfer
                .inner
                .lock()
                .map_err(|_| ApiError::new(500, "transfer unavailable"))?;
            state.touched = Instant::now();
            match (request.method.as_str(), operation) {
                ("GET", "") => {
                    validate_source(&state)?;
                    json_response(stream, 200, describe(id, &state))
                }
                ("PATCH", "") => {
                    if request.content_length > crate::config::get().transfer_chunk_max_bytes as u64
                    {
                        return Err(ApiError::new(413, "upload chunk exceeds configured limit"));
                    }
                    if state.state == "complete" || state.state == "cancelled" {
                        return Err(ApiError::new(409, "transfer is finished"));
                    }
                    check_source_query(request, &state)?;
                    let offset = query(&request.path, "offset")?
                        .ok_or_else(|| ApiError::new(400, "offset is required"))?
                        .parse::<u64>()
                        .map_err(|_| ApiError::new(400, "invalid offset"))?;
                    let upload = state
                        .upload
                        .as_ref()
                        .ok_or_else(|| ApiError::new(400, "transfer is not an upload"))?;
                    if offset != upload.offset {
                        return Err(ApiError::new(
                            409,
                            format!("offset mismatch; expected {}", upload.offset),
                        ));
                    }
                    let meta = state.file.metadata().map_err(io_error)?;
                    if meta.len() != offset
                        || identity(&meta) != upload.partial_identity
                        || !same_inode(
                            &meta,
                            &fs::symlink_metadata(anchored(
                                upload.stage_directory(),
                                &upload.temporary,
                            ))
                            .map_err(io_error)?,
                        )
                    {
                        return Err(ApiError::new(409, "partial destination changed"));
                    }
                    if request.content_length > state.size.saturating_sub(offset) {
                        return Err(ApiError::new(409, "chunk exceeds declared source size"));
                    }
                    if !request
                        .header("content-type")
                        .unwrap_or("")
                        .starts_with("application/octet-stream")
                    {
                        return Err(ApiError::new(
                            415,
                            "upload requires application/octet-stream",
                        ));
                    }
                    transfer.cancelled.store(false, Ordering::SeqCst);
                    state.state = "active";
                    state.file.seek(SeekFrom::Start(offset)).map_err(io_error)?;
                    let result = stream_upload(
                        request,
                        stream,
                        &mut state.file,
                        &transfer.cancelled,
                        stopping,
                    );
                    let metadata = state.file.metadata().map_err(io_error)?;
                    let upload = state.upload.as_mut().unwrap();
                    upload.offset = metadata.len();
                    upload.partial_identity = identity(&metadata);
                    state.state = if result.is_ok() { "ready" } else { "paused" };
                    state.touched = Instant::now();
                    result?;
                    json_response(stream, 200, describe(id, &state))
                }
                ("POST", "complete") => {
                    check_source_query_optional(request, &state)?;
                    if state.state == "complete" {
                        return json_response(stream, 200, describe(id, &state));
                    }
                    let upload = state
                        .upload
                        .as_ref()
                        .ok_or_else(|| ApiError::new(400, "transfer is not an upload"))?;
                    let meta = state.file.metadata().map_err(io_error)?;
                    if upload.offset != state.size || meta.len() != state.size {
                        return Err(ApiError::new(409, "upload is incomplete"));
                    }
                    if identity(&meta) != upload.partial_identity
                        || !same_inode(
                            &meta,
                            &fs::symlink_metadata(anchored(
                                upload.stage_directory(),
                                &upload.temporary,
                            ))
                            .map_err(io_error)?,
                        )
                    {
                        return Err(ApiError::new(409, "partial destination changed"));
                    }
                    let destination = anchored(&upload.directory, &upload.final_name);
                    if upload.replace {
                        let meta = fs::symlink_metadata(&destination).map_err(io_error)?;
                        if !meta.is_file() || Some(identity(&meta)) != upload.original {
                            return Err(ApiError::new(409, "destination changed during upload"));
                        }
                    }
                    state.file.sync_all().map_err(io_error)?;
                    if let Some(writable) = &upload.inplace {
                        if Some(identity(&writable.metadata().map_err(io_error)?))
                            != upload.original
                        {
                            return Err(ApiError::new(
                                409,
                                "opened destination changed during upload",
                            ));
                        }
                        let mut source = state.file.try_clone().map_err(io_error)?;
                        source.seek(SeekFrom::Start(0)).map_err(io_error)?;
                        let mut writable = writable.try_clone().map_err(io_error)?;
                        writable.seek(SeekFrom::Start(0)).map_err(io_error)?;
                        // This mode is explicitly chosen and preserves the existing inode.
                        // Do not truncate early: a short write still retains its old tail.
                        let mut buffer = [0; BUFFER];
                        let mut remaining = state.size;
                        while remaining > 0 {
                            if transfer.cancelled.load(Ordering::SeqCst)
                                || stopping.load(Ordering::SeqCst)
                            {
                                return Err(ApiError::new(409,"in-place replacement cancelled; destination may contain partial contents"));
                            }
                            let n = source
                                .read(&mut buffer[..remaining.min(BUFFER as u64) as usize])
                                .map_err(io_error)?;
                            if n == 0 {
                                return Err(ApiError::new(500, "staged source truncated"));
                            }
                            writable.write_all(&buffer[..n]).map_err(io_error)?;
                            remaining -= n as u64;
                        }
                        writable.set_len(state.size).map_err(io_error)?;
                        writable.sync_all().map_err(io_error)?;
                        fs::remove_file(anchored(upload.stage_directory(), &upload.temporary))
                            .map_err(io_error)?;
                    } else if upload.replace {
                        fs::rename(
                            anchored(upload.stage_directory(), &upload.temporary),
                            &destination,
                        )
                        .map_err(io_error)?;
                    } else {
                        fs::hard_link(
                            anchored(upload.stage_directory(), &upload.temporary),
                            &destination,
                        )
                        .map_err(io_error)?;
                        fs::remove_file(anchored(upload.stage_directory(), &upload.temporary))
                            .map_err(io_error)?;
                    }
                    if upload.inplace.is_none() {
                        if let Err(error) = upload.directory.sync_all() {
                            if error.raw_os_error() != Some(libc::EBADF) {
                                return Err(io_error(error));
                            }
                        }
                    }
                    state.upload = None;
                    state.state = "complete";
                    json_response(stream, 200, describe(id, &state))
                }
                ("GET", "download") => {
                    if state.upload.is_some() {
                        return Err(ApiError::new(400, "transfer is not a download"));
                    }
                    check_source_query(request, &state)?;
                    validate_source(&state)?;
                    let offset = query(&request.path, "offset")?
                        .unwrap_or_else(|| "0".into())
                        .parse::<u64>()
                        .map_err(|_| ApiError::new(400, "invalid offset"))?;
                    if offset > state.size {
                        return Err(ApiError::new(416, "offset is past end of source"));
                    }
                    transfer.cancelled.store(false, Ordering::SeqCst);
                    state.state = "active";
                    state.file.seek(SeekFrom::Start(offset)).map_err(io_error)?;
                    let length = state.size - offset;
                    write!(stream,"HTTP/1.1 {} OK\r\nContent-Type: application/octet-stream\r\nContent-Length: {length}\r\nETag: \"{}\"\r\nX-Source-Id: {}\r\nX-Transfer-Offset: {offset}\r\nConnection: close\r\nCache-Control: no-store\r\n\r\n",if offset==0{200}else{206},state.source_id,state.source_id).map_err(|e|ApiError::new(0,e.to_string()))?;
                    let mut buffer = [0; BUFFER];
                    let mut remaining = length;
                    while remaining > 0 {
                        if transfer.cancelled.load(Ordering::SeqCst)
                            || stopping.load(Ordering::SeqCst)
                        {
                            state.state = "paused";
                            return Err(ApiError::new(0, "download cancelled"));
                        }
                        let count = state
                            .file
                            .read(&mut buffer[..remaining.min(BUFFER as u64) as usize])
                            .map_err(|e| ApiError::new(0, e.to_string()))?;
                        if count == 0 {
                            state.state = "paused";
                            return Err(ApiError::new(0, "source truncated during download"));
                        }
                        stream
                            .write_all(&buffer[..count])
                            .map_err(|e| ApiError::new(0, e.to_string()))?;
                        remaining -= count as u64;
                    }
                    state.state = "ready";
                    state.touched = Instant::now();
                    validate_source(&state).map_err(|e| ApiError::new(0, e.message))?;
                    stream.flush().map_err(|e| ApiError::new(0, e.to_string()))
                }
                _ => Err(ApiError::new(404, "unknown transfer route")),
            }
        })())
    }
}
fn same_inode(a: &fs::Metadata, b: &fs::Metadata) -> bool {
    a.dev() == b.dev() && a.ino() == b.ino()
}
fn validate_source(state: &TransferInner) -> Result<(), ApiError> {
    if state.upload.is_none() && state.state != "complete" {
        let opened = state.file.metadata().map_err(io_error)?;
        let current = fs::metadata(&state.path).map_err(io_error)?;
        if state.source_id != identity(&opened) || !same_inode(&opened, &current) {
            return Err(ApiError::new(
                409,
                "download source changed; restart required",
            ));
        }
    }
    Ok(())
}
fn check_source_query(request: &Request, state: &TransferInner) -> Result<(), ApiError> {
    let source = query(&request.path, "source_id")?
        .ok_or_else(|| ApiError::new(400, "source_id is required"))?;
    if source != state.source_id {
        return Err(ApiError::new(
            409,
            "source identity changed; restart required",
        ));
    }
    Ok(())
}
fn check_source_query_optional(request: &Request, state: &TransferInner) -> Result<(), ApiError> {
    if query(&request.path, "source_id")?.is_some() {
        check_source_query(request, state)
    } else {
        Ok(())
    }
}
fn describe(id: &str, state: &TransferInner) -> serde_json::Value {
    json!({"id":id,"direction":if state.upload.is_some()||state.state=="complete"{"upload"}else{"download"},"path":state.path.to_str(),"path_id":hex(state.path.as_os_str().as_bytes()),"size":state.size,"offset":state.upload.as_ref().map(|u|u.offset).unwrap_or(if state.state=="complete"{state.size}else{0}),"source_id":state.source_id,"state":state.state,"atomic":state.atomic})
}
fn stream_upload(
    request: &Request,
    stream: &mut TcpStream,
    file: &mut fs::File,
    cancelled: &AtomicBool,
    stopping: &AtomicBool,
) -> Result<(), ApiError> {
    let mut prefix = Cursor::new(&request.body);
    let mut remaining = request.content_length;
    let mut buffer = [0; BUFFER];
    let mut last_progress = Instant::now();
    while remaining > 0 {
        if cancelled.load(Ordering::SeqCst) || stopping.load(Ordering::SeqCst) {
            return Err(ApiError::new(409, "upload cancelled; partial retained"));
        }
        let limit = remaining.min(BUFFER as u64) as usize;
        let count = if prefix.position() < request.body.len() as u64 {
            prefix.read(&mut buffer[..limit])
        } else {
            stream.read(&mut buffer[..limit])
        };
        let count = match count {
            Ok(0) => return Err(ApiError::new(400, "upload interrupted; partial retained")),
            Ok(n) => n,
            Err(e)
                if matches!(
                    e.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                ) =>
            {
                if last_progress.elapsed() > Duration::from_secs(120) {
                    return Err(ApiError::new(408, "upload timed out; partial retained"));
                }
                continue;
            }
            Err(e) => return Err(io_error(e)),
        };
        file.write_all(&buffer[..count]).map_err(io_error)?;
        remaining -= count as u64;
        last_progress = Instant::now();
    }
    file.sync_data().map_err(io_error)
}

/// Legacy filename routes remain usable, but stream rather than buffer entire files.
pub(crate) fn legacy_route(
    request: &Request,
    stream: &mut TcpStream,
    directory: &Path,
    name: &str,
    stopping: &AtomicBool,
) -> Result<(), ApiError> {
    let path = directory.join(name);
    if request.method == "GET" {
        let mut file = open_regular(&path)?;
        let size = file.metadata().map_err(io_error)?.len();
        write!(stream,"HTTP/1.1 200 OK\r\nContent-Type: application/octet-stream\r\nContent-Length: {size}\r\nConnection: close\r\n\r\n").map_err(|e|ApiError::new(0,e.to_string()))?;
        let mut buffer = [0; BUFFER];
        let mut remaining = size;
        while remaining > 0 {
            if stopping.load(Ordering::SeqCst) {
                return Err(ApiError::new(0, "host stopping"));
            }
            let n = file
                .read(&mut buffer[..remaining.min(BUFFER as u64) as usize])
                .map_err(|e| ApiError::new(0, e.to_string()))?;
            if n == 0 {
                return Err(ApiError::new(0, "source truncated"));
            }
            stream
                .write_all(&buffer[..n])
                .map_err(|e| ApiError::new(0, e.to_string()))?;
            remaining -= n as u64;
        }
        Ok(())
    } else {
        if !request
            .header("content-type")
            .unwrap_or("")
            .starts_with("application/octet-stream")
        {
            return Err(ApiError::new(
                415,
                "file upload requires application/octet-stream",
            ));
        }
        let manager = TransferManager::default();
        let existed = path.exists();
        let (id, transfer) = manager.create(Start {
            destination: PathSpec {
                path: path.to_str().map(str::to_owned),
                path_id: None,
            },
            direction: "upload".into(),
            size: Some(request.content_length),
            source_id: Some("legacy".into()),
            conflict: "replace".into(),
        })?;
        let mut state = transfer
            .inner
            .lock()
            .map_err(|_| ApiError::new(500, "transfer unavailable"))?;
        stream_upload(
            request,
            stream,
            &mut state.file,
            &transfer.cancelled,
            stopping,
        )?;
        let upload = state.upload.as_ref().unwrap();
        state.file.sync_all().map_err(io_error)?;
        fs::rename(
            anchored(upload.stage_directory(), &upload.temporary),
            anchored(&upload.directory, &upload.final_name),
        )
        .map_err(io_error)?;
        state.upload = None;
        state.state = "complete";
        drop(state);
        drop(id);
        json_response(stream, if existed { 200 } else { 201 }, json!({"ok":true}))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::env;
    use std::net::{Shutdown, TcpListener};
    struct Scratch(PathBuf);
    impl Scratch {
        fn new() -> Self {
            let path = env::temp_dir().join(format!(
                "windowlink-files-test-{}-{}",
                std::process::id(),
                NEXT_TRANSFER.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir(&path).unwrap();
            Self(path)
        }
    }
    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn sorted_listing_paginates_global_order_in_both_directions() {
        let scratch = Scratch::new();
        for (name, size, seconds, nanos) in [
            ("z", 9, 30, 0),
            ("a", 1, 40, 0),
            ("m", 5, 10, 2),
            ("n", 5, 10, 1),
        ] {
            let file = fs::File::create(scratch.0.join(name)).unwrap();
            file.set_len(size).unwrap();
            file.set_times(
                fs::FileTimes::new().set_modified(UNIX_EPOCH + Duration::new(seconds, nanos)),
            )
            .unwrap();
        }
        fs::write(scratch.0.join(".windowlink-upload-hidden"), b"hidden").unwrap();
        let directory = open_directory(&scratch.0).unwrap();
        for (key, ascending, descending) in [
            ("name", ["a", "m", "n", "z"], ["z", "n", "m", "a"]),
            ("size", ["a", "m", "n", "z"], ["z", "m", "n", "a"]),
            ("modified", ["n", "m", "z", "a"], ["a", "z", "m", "n"]),
            ("type", ["a", "m", "n", "z"], ["a", "m", "n", "z"]),
        ] {
            for (reverse, expected) in [(false, ascending), (true, descending)] {
                let order =
                    listing_order(&format!("/fs/list?sort={key}&descending={reverse}")).unwrap();
                let (first, next) = list_directory(&scratch.0, &directory, 0, 2, order).unwrap();
                assert_eq!(next, Some(2));
                let (second, next) =
                    list_directory(&scratch.0, &directory, next.unwrap(), 2, order).unwrap();
                assert_eq!(next, None);
                let names: Vec<_> = first
                    .iter()
                    .chain(&second)
                    .map(|e| e.name.as_str())
                    .collect();
                assert_eq!(names, expected, "sort={key}, descending={reverse}");
                let (past_end, next) =
                    list_directory(&scratch.0, &directory, usize::MAX, 2, order).unwrap();
                assert!(past_end.is_empty());
                assert_eq!(next, None);
            }
        }
    }

    #[test]
    fn sorted_listing_uses_kind_and_stable_raw_path_ties() {
        let scratch = Scratch::new();
        fs::create_dir(scratch.0.join("directory")).unwrap();
        let raw_names = [OsStr::from_bytes(b"x\xfe"), OsStr::from_bytes(b"x\xff")];
        for name in raw_names.iter().rev() {
            fs::write(scratch.0.join(name), b"x").unwrap();
        }
        std::os::unix::fs::symlink("directory", scratch.0.join("link")).unwrap();
        let directory = open_directory(&scratch.0).unwrap();
        for (reverse, kinds) in [
            (false, ["directory", "file", "file", "symlink"]),
            (true, ["symlink", "file", "file", "directory"]),
        ] {
            let (entries, next) = list_directory(
                &scratch.0,
                &directory,
                0,
                10,
                Some((SortKey::Type, reverse)),
            )
            .unwrap();
            assert_eq!(next, None);
            assert_eq!(entries.iter().map(|e| e.kind).collect::<Vec<_>>(), kinds);
            assert_eq!(entries[1].name, entries[2].name);
            assert_eq!(
                entries[1].path_id,
                hex(scratch.0.join(raw_names[0]).as_os_str().as_bytes())
            );
            assert_eq!(
                entries[2].path_id,
                hex(scratch.0.join(raw_names[1]).as_os_str().as_bytes())
            );
            let (entries, _) = list_directory(
                &scratch.0,
                &directory,
                0,
                10,
                Some((SortKey::Name, reverse)),
            )
            .unwrap();
            let raw_entries: Vec<_> = entries.iter().filter(|e| e.path.is_none()).collect();
            assert!(raw_entries[0].path_id < raw_entries[1].path_id);
        }
    }

    #[test]
    fn listing_sort_query_validates_options_and_preserves_default_order() {
        assert_eq!(listing_order("/fs/list").unwrap(), None);
        assert_eq!(
            listing_order("/fs/list?sort=name").unwrap(),
            Some((SortKey::Name, false))
        );
        assert_eq!(listing_order("/fs/list?descending=true").unwrap(), None);
        for path in ["/fs/list?sort=unknown", "/fs/list?sort=size&descending=1"] {
            assert_eq!(listing_order(path).unwrap_err().status, 400);
        }
    }

    fn start(path: &Path, direction: &str, size: u64, conflict: &str) -> Start {
        Start {
            destination: PathSpec {
                path: path.to_str().map(str::to_owned),
                path_id: None,
            },
            direction: direction.into(),
            size: Some(size),
            source_id: Some("phone-source".into()),
            conflict: conflict.into(),
        }
    }
    fn request(method: &str, path: String, body: Vec<u8>, length: u64) -> Request {
        Request {
            method: method.into(),
            path,
            headers: vec![("content-type".into(), "application/octet-stream".into())],
            body,
            content_length: length,
        }
    }
    fn exchange(manager: Arc<TransferManager>, request: Request, wire: &[u8]) -> (u16, Vec<u8>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let stopping = AtomicBool::new(false);
            if let Err(e) = manager.route(&request, &mut stream, &stopping).unwrap() {
                if e.status != 0 {
                    send_bytes(
                        &mut stream,
                        e.status,
                        "application/json",
                        json!({"error":e.message}).to_string().as_bytes(),
                    )
                    .unwrap();
                }
            }
        });
        let mut client = TcpStream::connect(address).unwrap();
        client.write_all(wire).unwrap();
        client.shutdown(Shutdown::Write).unwrap();
        let mut response = Vec::new();
        let mut byte = [0];
        while !response.ends_with(b"\r\n\r\n") {
            client.read_exact(&mut byte).unwrap();
            response.push(byte[0]);
        }
        let header = std::str::from_utf8(&response).unwrap();
        let length: usize = header
            .lines()
            .find_map(|line| line.strip_prefix("Content-Length: "))
            .unwrap()
            .parse()
            .unwrap();
        let start = response.len();
        response.resize(start + length, 0);
        client.read_exact(&mut response[start..]).unwrap();
        server.join().unwrap();
        let end = response.windows(4).position(|v| v == b"\r\n\r\n").unwrap();
        let status = std::str::from_utf8(&response[..end])
            .unwrap()
            .split_whitespace()
            .nth(1)
            .unwrap()
            .parse()
            .unwrap();
        (status, response[end + 4..].to_vec())
    }
    #[test]
    fn interrupted_upload_resumes_and_finalizes_atomically() {
        let scratch = Scratch::new();
        let path = scratch.0.join("quoted ' Unicode 日本.txt");
        let manager = Arc::new(TransferManager::default());
        let (id, transfer) = manager.create(start(&path, "upload", 8, "skip")).unwrap();
        let (status, _) = exchange(
            manager.clone(),
            request(
                "PATCH",
                format!("/transfers/{id}?offset=0&source_id=phone-source"),
                vec![],
                8,
            ),
            b"abc",
        );
        assert_eq!(status, 400);
        assert!(!path.exists());
        assert_eq!(
            transfer
                .inner
                .lock()
                .unwrap()
                .upload
                .as_ref()
                .unwrap()
                .offset,
            3
        );
        assert_eq!(
            exchange(
                manager.clone(),
                request("DELETE", format!("/transfers/{id}?retain=true"), vec![], 0),
                b""
            )
            .0,
            200
        );
        assert_eq!(
            exchange(
                manager.clone(),
                request(
                    "PATCH",
                    format!("/transfers/{id}?offset=3&source_id=changed"),
                    vec![],
                    5
                ),
                b"defgh"
            )
            .0,
            409
        );
        assert_eq!(
            exchange(
                manager.clone(),
                request(
                    "PATCH",
                    format!("/transfers/{id}?offset=3&source_id=phone-source"),
                    vec![],
                    5
                ),
                b"defgh"
            )
            .0,
            200
        );
        assert_eq!(
            exchange(
                manager.clone(),
                request("POST", format!("/transfers/{id}/complete"), vec![], 0),
                b""
            )
            .0,
            200
        );
        assert_eq!(fs::read(&path).unwrap(), b"abcdefgh");
    }
    #[test]
    fn changed_source_partial_and_destination_are_rejected() {
        let scratch = Scratch::new();
        let path = scratch.0.join("existing");
        fs::write(&path, b"old").unwrap();
        let manager = Arc::new(TransferManager::default());
        let (id, transfer) = manager
            .create(start(&path, "upload", 3, "replace"))
            .unwrap();
        assert_eq!(
            exchange(
                manager.clone(),
                request(
                    "PATCH",
                    format!("/transfers/{id}?offset=0&source_id=phone-source"),
                    b"new".to_vec(),
                    3
                ),
                b""
            )
            .0,
            200
        );
        fs::write(&path, b"changed").unwrap();
        assert_eq!(
            exchange(
                manager.clone(),
                request("POST", format!("/transfers/{id}/complete"), vec![], 0),
                b""
            )
            .0,
            409
        );
        assert_eq!(fs::read(&path).unwrap(), b"changed");
        let stage = {
            let state = transfer.inner.lock().unwrap();
            let upload = state.upload.as_ref().unwrap();
            anchored(upload.stage_directory(), &upload.temporary)
        };
        fs::write(stage, b"bad").unwrap();
        assert_eq!(
            exchange(
                manager.clone(),
                request("POST", format!("/transfers/{id}/complete"), vec![], 0),
                b""
            )
            .0,
            409
        );
        let (download, _) = manager.create(start(&path, "download", 0, "skip")).unwrap();
        fs::write(&path, b"changed again").unwrap();
        assert_eq!(
            exchange(
                manager.clone(),
                request("GET", format!("/transfers/{download}"), vec![], 0),
                b""
            )
            .0,
            409
        );
    }
    #[test]
    fn conflicts_cancellation_raw_names_and_special_files() {
        let scratch = Scratch::new();
        let path = scratch.0.join("existing");
        fs::write(&path, b"original").unwrap();
        let manager = Arc::new(TransferManager::default());
        assert_eq!(
            manager
                .create(start(&path, "upload", 0, "skip"))
                .err()
                .unwrap()
                .status,
            409
        );
        let (id, transfer) = manager
            .create(start(&path, "upload", 0, "keep_both"))
            .unwrap();
        assert_ne!(transfer.inner.lock().unwrap().path, path);
        assert_eq!(
            exchange(
                manager.clone(),
                request("POST", format!("/transfers/{id}/complete"), vec![], 0),
                b""
            )
            .0,
            200
        );
        assert_eq!(fs::read(&path).unwrap(), b"original");
        let (id, transfer) = manager
            .create(start(&scratch.0.join("cancel"), "upload", 8, "skip"))
            .unwrap();
        let temporary = {
            let state = transfer.inner.lock().unwrap();
            let upload = state.upload.as_ref().unwrap();
            scratch.0.join(&upload.temporary)
        };
        assert!(temporary.exists());
        assert_eq!(
            exchange(
                manager.clone(),
                request("DELETE", format!("/transfers/{id}?retain=false"), vec![], 0),
                b""
            )
            .0,
            200
        );
        assert!(!temporary.exists());
        let raw = scratch.0.join(OsStr::from_bytes(b"hidden-\xff"));
        fs::write(&raw, b"raw").unwrap();
        assert_eq!(decode_id(&hex(raw.as_os_str().as_bytes())).unwrap(), raw);
        let fifo = scratch.0.join("fifo");
        let c = std::ffi::CString::new(fifo.as_os_str().as_bytes()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o600) }, 0);
        assert_eq!(open_regular(&fifo).err().unwrap().status, 415);
        let symlink = scratch.0.join("link");
        std::os::unix::fs::symlink(&path, &symlink).unwrap();
        assert_eq!(
            manager
                .create(start(&symlink, "upload", 0, "replace"))
                .err()
                .unwrap()
                .status,
            409
        );
    }
    #[test]
    fn explicit_in_place_replace_obeys_file_permissions_in_restricted_directory() {
        let scratch = Scratch::new();
        let directory = scratch.0.join("restricted");
        fs::create_dir(&directory).unwrap();
        let path = directory.join("writable");
        fs::write(&path, b"old contents").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        fs::set_permissions(&directory, fs::Permissions::from_mode(0o100)).unwrap();
        let manager = Arc::new(TransferManager::default());
        assert_eq!(
            manager
                .create(start(&path, "upload", 3, "replace"))
                .err()
                .unwrap()
                .status,
            403
        );
        let inode = fs::metadata(&path).unwrap().ino();
        let (id, transfer) = manager
            .create(start(&path, "upload", 3, "replace_in_place"))
            .unwrap();
        assert!(!transfer.inner.lock().unwrap().atomic);
        assert_eq!(fs::read(&path).unwrap(), b"old contents");
        assert_eq!(
            exchange(
                manager.clone(),
                request(
                    "PATCH",
                    format!("/transfers/{id}?offset=0&source_id=phone-source"),
                    b"new".to_vec(),
                    3
                ),
                b""
            )
            .0,
            200
        );
        assert_eq!(
            exchange(
                manager.clone(),
                request("POST", format!("/transfers/{id}/complete"), vec![], 0),
                b""
            )
            .0,
            200
        );
        assert_eq!(fs::read(&path).unwrap(), b"new");
        assert_eq!(fs::metadata(&path).unwrap().ino(), inode);
        fs::set_permissions(&path, fs::Permissions::from_mode(0o400)).unwrap();
        assert_eq!(
            manager
                .create(start(&path, "upload", 3, "replace_in_place"))
                .err()
                .unwrap()
                .status,
            403
        );
        fs::set_permissions(&directory, fs::Permissions::from_mode(0o700)).unwrap();
    }

    #[test]
    fn file_larger_than_old_limit_streams_with_bounded_buffers() {
        let scratch = Scratch::new();
        let path = scratch.0.join("large");
        let size = 101 * 1024 * 1024 + 13;
        fs::File::create(&path).unwrap().set_len(size).unwrap();
        let manager = Arc::new(TransferManager::default());
        let (id, transfer) = manager.create(start(&path, "download", 0, "skip")).unwrap();
        let source = transfer.inner.lock().unwrap().source_id.clone();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            manager
                .route(
                    &request(
                        "GET",
                        format!("/transfers/{id}/download?source_id={source}"),
                        vec![],
                        0,
                    ),
                    &mut stream,
                    &AtomicBool::new(false),
                )
                .unwrap()
                .unwrap();
        });
        let mut client = TcpStream::connect(address).unwrap();
        let mut header = Vec::new();
        let mut byte = [0];
        while !header.ends_with(b"\r\n\r\n") {
            client.read_exact(&mut byte).unwrap();
            header.push(byte[0]);
        }
        assert!(String::from_utf8(header)
            .unwrap()
            .contains(&format!("Content-Length: {size}")));
        let mut buffer = [0; BUFFER];
        let mut received = 0u64;
        loop {
            let n = client.read(&mut buffer).unwrap();
            if n == 0 {
                break;
            }
            assert!(buffer[..n].iter().all(|b| *b == 0));
            received += n as u64;
        }
        assert_eq!(received, size);
        server.join().unwrap();
    }
}
