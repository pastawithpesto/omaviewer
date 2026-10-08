const MAX_CLIPBOARD_BYTES: usize = 1024 * 1024;

pub fn read_remote(host: &str, port: u16, token: &str) -> Result<String, String> {
    let (_, body) = crate::window_picker::request(host, port, token, "GET", "/clipboard", "", &[])?;
    if body.len() > MAX_CLIPBOARD_BYTES {
        return Err("Desktop clipboard text exceeds 1 MiB".into());
    }
    String::from_utf8(body).map_err(|_| "Desktop clipboard is not valid UTF-8".into())
}

pub fn write_remote(host: &str, port: u16, token: &str, text: &str) -> Result<(), String> {
    if text.len() > MAX_CLIPBOARD_BYTES {
        return Err("Clipboard text exceeds 1 MiB".into());
    }
    crate::window_picker::request(
        host,
        port,
        token,
        "POST",
        "/clipboard",
        "text/plain; charset=utf-8",
        text.as_bytes(),
    )?;
    Ok(())
}

#[cfg(target_os = "android")]
#[derive(Clone)]
pub struct AndroidClipboard {
    app: android_activity::AndroidApp,
}

#[cfg(target_os = "android")]
impl AndroidClipboard {
    pub fn new(app: android_activity::AndroidApp) -> Self {
        Self { app }
    }

    /// NativeActivity updates this rectangle when the phone keyboard covers
    /// the surface. Winit currently does not propagate that change to egui.
    pub fn visible_content_bottom(&self) -> i32 {
        self.app.content_rect().bottom
    }

    pub fn has_focus(&self) -> Result<bool, String> {
        self.native(|env, activity| {
            env.call_method(
                activity,
                jni::jni_str!("hasWindowFocus"),
                jni::jni_sig!("()Z"),
                &[],
            )?
            .z()
        })
    }

    pub fn set_text(&self, text: &str) -> Result<(), String> {
        use jni::{
            JValue, JavaVM,
            objects::{Global, JObject},
        };

        let vm = unsafe { JavaVM::from_raw(self.app.vm_as_ptr().cast()) };
        let activity_raw = self.app.activity_as_ptr() as jni::sys::jobject;
        vm.attach_current_thread(|env| -> jni::errors::Result<()> {
            let activity = unsafe { env.as_cast_raw::<Global<JObject>>(&activity_raw)? };
            let activity = activity.as_ref();
            let service_name = env.new_string("clipboard")?;
            let manager = env
                .call_method(
                    activity,
                    jni::jni_str!("getSystemService"),
                    jni::jni_sig!("(Ljava/lang/String;)Ljava/lang/Object;"),
                    &[JValue::Object(&service_name)],
                )?
                .l()?;
            let label = env.new_string("OmaLink")?;
            let clip_text = env.new_string(text)?;
            let clip_data = env
                .call_static_method(
                    jni::jni_str!("android/content/ClipData"),
                    jni::jni_str!("newPlainText"),
                    jni::jni_sig!("(Ljava/lang/CharSequence;Ljava/lang/CharSequence;)Landroid/content/ClipData;"),
                    &[JValue::Object(&label), JValue::Object(&clip_text)],
                )?
                .l()?;
            env.call_method(
                &manager,
                jni::jni_str!("setPrimaryClip"),
                jni::jni_sig!("(Landroid/content/ClipData;)V"),
                &[JValue::Object(&clip_data)],
            )?;
            Ok(())
        })
        .map_err(|error| format!("Could not copy to Android clipboard: {error}"))
    }

    pub fn get_text(&self) -> Result<String, String> {
        use jni::{
            JValue, JavaVM,
            objects::{Global, JObject, JString},
        };

        let vm = unsafe { JavaVM::from_raw(self.app.vm_as_ptr().cast()) };
        let activity_raw = self.app.activity_as_ptr() as jni::sys::jobject;
        vm.attach_current_thread(|env| -> jni::errors::Result<String> {
            let activity = unsafe { env.as_cast_raw::<Global<JObject>>(&activity_raw)? };
            let activity = activity.as_ref();
            let service_name = env.new_string("clipboard")?;
            let manager = env
                .call_method(
                    activity,
                    jni::jni_str!("getSystemService"),
                    jni::jni_sig!("(Ljava/lang/String;)Ljava/lang/Object;"),
                    &[JValue::Object(&service_name)],
                )?
                .l()?;
            let has_clip = env
                .call_method(
                    &manager,
                    jni::jni_str!("hasPrimaryClip"),
                    jni::jni_sig!("()Z"),
                    &[],
                )?
                .z()?;
            if !has_clip {
                return Ok(String::new());
            }
            let clip = env
                .call_method(
                    &manager,
                    jni::jni_str!("getPrimaryClip"),
                    jni::jni_sig!("()Landroid/content/ClipData;"),
                    &[],
                )?
                .l()?;
            if clip.is_null() {
                return Ok(String::new());
            }
            let item = env
                .call_method(
                    &clip,
                    jni::jni_str!("getItemAt"),
                    jni::jni_sig!("(I)Landroid/content/ClipData$Item;"),
                    &[JValue::Int(0)],
                )?
                .l()?;
            let text = env
                .call_method(
                    &item,
                    jni::jni_str!("getText"),
                    jni::jni_sig!("()Ljava/lang/CharSequence;"),
                    &[],
                )?
                .l()?;
            if text.is_null() {
                return Ok(String::new());
            }
            let string = env
                .call_method(
                    &text,
                    jni::jni_str!("toString"),
                    jni::jni_sig!("()Ljava/lang/String;"),
                    &[],
                )?
                .l()?;
            let string = unsafe { JString::from_raw(env, string.into_raw()) };
            Ok(string.mutf8_chars(env)?.to_string())
        })
        .map_err(|error| format!("Could not read Android clipboard: {error}"))
    }

    pub fn start_file_picker(&self, save: bool, suggested_name: &str) -> Result<(), String> {
        use jni::{
            JValue, JavaVM,
            objects::{Global, JObject},
        };
        let vm = unsafe { JavaVM::from_raw(self.app.vm_as_ptr().cast()) };
        let activity_raw = self.app.activity_as_ptr() as jni::sys::jobject;
        vm.attach_current_thread(|env| -> jni::errors::Result<()> {
            let activity = unsafe { env.as_cast_raw::<Global<JObject>>(&activity_raw)? };
            let activity = activity.as_ref();
            let name = env.new_string(suggested_name)?;
            env.call_method(
                activity,
                jni::jni_str!("startWindowLinkFilePicker"),
                jni::jni_sig!("(ZLjava/lang/String;)V"),
                &[JValue::Bool(save), JValue::Object(&name)],
            )?;
            Ok(())
        })
        .map_err(|error| format!("Could not open Android file picker: {error}"))
    }

    pub fn take_file_result(&self) -> Result<Option<(bool, String)>, String> {
        use jni::{
            JavaVM,
            objects::{Global, JObject, JString},
        };
        let vm = unsafe { JavaVM::from_raw(self.app.vm_as_ptr().cast()) };
        let activity_raw = self.app.activity_as_ptr() as jni::sys::jobject;
        vm.attach_current_thread(|env| -> jni::errors::Result<Option<(bool, String)>> {
            let activity = unsafe { env.as_cast_raw::<Global<JObject>>(&activity_raw)? };
            let result = env
                .call_method(
                    activity.as_ref(),
                    jni::jni_str!("takeWindowLinkFileResult"),
                    jni::jni_sig!("()Ljava/lang/String;"),
                    &[],
                )?
                .l()?;
            if result.is_null() {
                return Ok(None);
            }
            let result = unsafe { JString::from_raw(env, result.into_raw()) };
            let result = result.mutf8_chars(env)?.to_string();
            let Some((kind, uri)) = result.split_once('\n') else {
                return Err(jni::errors::Error::NullPtr("malformed file picker result"));
            };
            Ok(Some((kind == "save", uri.to_owned())))
        })
        .map_err(|error| format!("Could not read Android file picker result: {error}"))
    }

    pub fn read_file(&self, uri: &str) -> Result<Vec<u8>, String> {
        use jni::{
            JavaVM,
            objects::{Global, JObject},
        };
        let vm = unsafe { JavaVM::from_raw(self.app.vm_as_ptr().cast()) };
        let activity_raw = self.app.activity_as_ptr() as jni::sys::jobject;
        vm.attach_current_thread(|env| -> jni::errors::Result<Vec<u8>> {
            let activity = unsafe { env.as_cast_raw::<Global<JObject>>(&activity_raw)? };
            let uri = env.new_string(uri)?;
            let bytes = env
                .call_method(
                    activity.as_ref(),
                    jni::jni_str!("readWindowLinkFile"),
                    jni::jni_sig!("(Ljava/lang/String;)[B"),
                    &[jni::JValue::Object(&uri)],
                )?
                .l()?;
            let bytes = unsafe { jni::objects::JByteArray::from_raw(env, bytes.into_raw()) };
            env.convert_byte_array(&bytes)
        })
        .map_err(|error| format!("Could not read selected file: {error}"))
    }

    pub fn write_file(&self, uri: &str, contents: &[u8]) -> Result<(), String> {
        use jni::{
            JavaVM,
            objects::{Global, JObject},
        };
        let vm = unsafe { JavaVM::from_raw(self.app.vm_as_ptr().cast()) };
        let activity_raw = self.app.activity_as_ptr() as jni::sys::jobject;
        vm.attach_current_thread(|env| -> jni::errors::Result<()> {
            let activity = unsafe { env.as_cast_raw::<Global<JObject>>(&activity_raw)? };
            let uri = env.new_string(uri)?;
            let contents = env.byte_array_from_slice(contents)?;
            env.call_method(
                activity.as_ref(),
                jni::jni_str!("writeWindowLinkFile"),
                jni::jni_sig!("(Ljava/lang/String;[B)V"),
                &[jni::JValue::Object(&uri), jni::JValue::Object(&contents)],
            )?;
            Ok(())
        })
        .map_err(|error| format!("Could not save downloaded file: {error}"))
    }

    pub fn file_display_name(&self, uri: &str) -> Result<String, String> {
        self.call_uri_string(uri)
    }

    fn call_uri_string(&self, uri: &str) -> Result<String, String> {
        use jni::{
            JavaVM,
            objects::{Global, JObject, JString},
        };
        let vm = unsafe { JavaVM::from_raw(self.app.vm_as_ptr().cast()) };
        let activity_raw = self.app.activity_as_ptr() as jni::sys::jobject;
        vm.attach_current_thread(|env| -> jni::errors::Result<String> {
            let activity = unsafe { env.as_cast_raw::<Global<JObject>>(&activity_raw)? };
            let uri = env.new_string(uri)?;
            let result = env
                .call_method(
                    activity.as_ref(),
                    jni::jni_str!("windowLinkDisplayName"),
                    jni::jni_sig!("(Ljava/lang/String;)Ljava/lang/String;"),
                    &[jni::JValue::Object(&uri)],
                )?
                .l()?;
            let result = unsafe { JString::from_raw(env, result.into_raw()) };
            Ok(result.mutf8_chars(env)?.to_string())
        })
        .map_err(|error| format!("Could not read selected filename: {error}"))
    }
}

#[cfg(target_os = "android")]
impl AndroidClipboard {
    fn native<T>(
        &self,
        callback: impl FnOnce(&mut jni::Env, &jni::objects::JObject) -> jni::errors::Result<T>,
    ) -> Result<T, String> {
        let vm = unsafe { jni::JavaVM::from_raw(self.app.vm_as_ptr().cast()) };
        let raw = self.app.activity_as_ptr() as jni::sys::jobject;
        vm.attach_current_thread(|env| {
            let activity =
                unsafe { env.as_cast_raw::<jni::objects::Global<jni::objects::JObject>>(&raw)? };
            callback(env, activity.as_ref())
        })
        .map_err(|e| format!("Android document/clipboard operation failed: {e}"))
    }
    fn native_string0(&self, name: &jni::strings::JNIStr) -> Result<Option<String>, String> {
        self.native(|env, activity| {
            let value = env
                .call_method(activity, name, jni::jni_sig!("()Ljava/lang/String;"), &[])?
                .l()?;
            if value.is_null() {
                return Ok(None);
            }
            let value = unsafe { jni::objects::JString::from_raw(env, value.into_raw()) };
            Ok(Some(value.mutf8_chars(env)?.to_string()))
        })
    }
    fn native_string1(&self, name: &jni::strings::JNIStr, text: &str) -> Result<String, String> {
        self.native(|env, activity| {
            let text = env.new_string(text)?;
            let value = env
                .call_method(
                    activity,
                    name,
                    jni::jni_sig!("(Ljava/lang/String;)Ljava/lang/String;"),
                    &[jni::JValue::Object(&text)],
                )?
                .l()?;
            let value = unsafe { jni::objects::JString::from_raw(env, value.into_raw()) };
            Ok(value.mutf8_chars(env)?.to_string())
        })
    }
    pub fn start_transfer_picker(
        &self,
        kind: &str,
        name: &str,
        generation: &str,
    ) -> Result<(), String> {
        self.native(|env, activity| {
            let kind = env.new_string(kind)?;
            let name = env.new_string(name)?;
            let generation = env.new_string(generation)?;
            env.call_method(
                activity,
                jni::jni_str!("startWindowLinkTransferPicker"),
                jni::jni_sig!("(Ljava/lang/String;Ljava/lang/String;Ljava/lang/String;)V"),
                &[
                    jni::JValue::Object(&kind),
                    jni::JValue::Object(&name),
                    jni::JValue::Object(&generation),
                ],
            )?;
            Ok(())
        })
    }
    pub fn take_transfer_result(&self) -> Result<Option<serde_json::Value>, String> {
        self.native_string0(jni::jni_str!("takeWindowLinkTransferResult"))?
            .map(|v| serde_json::from_str(&v).map_err(|e| e.to_string()))
            .transpose()
    }
    pub fn take_shared_uris(&self) -> Result<Vec<String>, String> {
        serde_json::from_str(
            &self
                .native_string0(jni::jni_str!("takeWindowLinkSharedUris"))?
                .unwrap_or_else(|| "[]".into()),
        )
        .map_err(|e| e.to_string())
    }
    pub fn cache_directory(&self) -> Result<std::path::PathBuf, String> {
        self.native_string0(jni::jni_str!("windowLinkCacheDirectory"))?
            .map(std::path::PathBuf::from)
            .ok_or_else(|| "Android cache is unavailable".into())
    }
    pub fn document_metadata(&self, uri: &str) -> Result<serde_json::Value, String> {
        serde_json::from_str(
            &self.native_string1(jni::jni_str!("windowLinkDocumentMetadata"), uri)?,
        )
        .map_err(|e| e.to_string())
    }
    pub fn open_document(&self, uri: &str, mode: &str) -> Result<std::fs::File, String> {
        use std::os::fd::FromRawFd;
        let fd = self.native(|env, activity| {
            let uri = env.new_string(uri)?;
            let mode = env.new_string(mode)?;
            env.call_method(
                activity,
                jni::jni_str!("openWindowLinkDocument"),
                jni::jni_sig!("(Ljava/lang/String;Ljava/lang/String;)I"),
                &[jni::JValue::Object(&uri), jni::JValue::Object(&mode)],
            )?
            .i()
        })?;
        if fd < 0 {
            return Err("Could not open document".into());
        }
        Ok(unsafe { std::fs::File::from_raw_fd(fd) })
    }
    pub fn tree_entries(&self, uri: &str, offset: i32) -> Result<serde_json::Value, String> {
        let text = self.native(|env, activity| {
            let uri = env.new_string(uri)?;
            let result = env
                .call_method(
                    activity,
                    jni::jni_str!("windowLinkTreeEntries"),
                    jni::jni_sig!("(Ljava/lang/String;I)Ljava/lang/String;"),
                    &[jni::JValue::Object(&uri), jni::JValue::Int(offset)],
                )?
                .l()?;
            let result = unsafe { jni::objects::JString::from_raw(env, result.into_raw()) };
            Ok(result.mutf8_chars(env)?.to_string())
        })?;
        serde_json::from_str(&text).map_err(|e| e.to_string())
    }
    pub fn create_document(
        &self,
        parent: &str,
        name: &str,
        directory: bool,
    ) -> Result<String, String> {
        self.native(|env, activity| {
            let parent = env.new_string(parent)?;
            let name = env.new_string(name)?;
            let result = env
                .call_method(
                    activity,
                    jni::jni_str!("createWindowLinkDocument"),
                    jni::jni_sig!("(Ljava/lang/String;Ljava/lang/String;Z)Ljava/lang/String;"),
                    &[
                        jni::JValue::Object(&parent),
                        jni::JValue::Object(&name),
                        jni::JValue::Bool(directory),
                    ],
                )?
                .l()?;
            let result = unsafe { jni::objects::JString::from_raw(env, result.into_raw()) };
            Ok(result.mutf8_chars(env)?.to_string())
        })
    }
    pub fn set_image(&self, bytes: &[u8]) -> Result<(), String> {
        if bytes.len() > 32 * 1024 * 1024 {
            return Err("Clipboard image exceeds 32 MiB".into());
        }
        self.native(|env, activity| {
            let bytes = env.byte_array_from_slice(bytes)?;
            env.call_method(
                activity,
                jni::jni_str!("setWindowLinkImageClipboard"),
                jni::jni_sig!("([B)V"),
                &[jni::JValue::Object(&bytes)],
            )?;
            Ok(())
        })
    }
    fn typed_payload(
        &self,
        text_enabled: bool,
        image_enabled: bool,
    ) -> Result<TypedPayload, String> {
        use std::io::Read;
        let value: serde_json::Value = serde_json::from_str(
            &self
                .native_string0(jni::jni_str!("windowLinkClipboardDescriptor"))?
                .unwrap_or_else(|| "{}".into()),
        )
        .map_err(|e| e.to_string())?;
        match value["mime"].as_str() {
            Some(mime)
                if (mime.starts_with("text/plain") && !text_enabled)
                    || (mime.starts_with("image/") && !image_enabled) =>
            {
                Ok(TypedPayload {
                    mime: None,
                    bytes: vec![],
                })
            }
            Some(mime) if mime.starts_with("text/plain") => {
                let bytes = value["text"].as_str().unwrap_or("").as_bytes().to_vec();
                if bytes.len() > MAX_CLIPBOARD_BYTES {
                    return Err("Clipboard text exceeds 1 MiB".into());
                }
                Ok(TypedPayload {
                    mime: Some("text/plain; charset=utf-8".into()),
                    bytes,
                })
            }
            Some(mime) if mime == "image/png" || mime == "image/jpeg" => {
                let uri = value["uri"]
                    .as_str()
                    .ok_or_else(|| "Clipboard image has no URI".to_owned())?;
                let mut bytes = Vec::new();
                self.open_document(uri, "r")?
                    .take((IMAGE_CLIPBOARD_MAX + 1) as u64)
                    .read_to_end(&mut bytes)
                    .map_err(|e| e.to_string())?;
                Ok(TypedPayload {
                    mime: Some("image/png".into()),
                    bytes: normalize_clipboard_image(mime, bytes)?,
                })
            }
            Some(mime) if mime.starts_with("image/") => Err(format!(
                "Clipboard image type {mime} is unsupported; use Share"
            )),
            _ => Ok(TypedPayload {
                mime: None,
                bytes: vec![],
            }),
        }
    }
}

const IMAGE_CLIPBOARD_MAX: usize = 32 * 1024 * 1024;
struct TypedPayload {
    mime: Option<String>,
    bytes: Vec<u8>,
}
impl TypedPayload {
    fn identity(&self) -> String {
        use sha2::{Digest, Sha256};
        let mut hash = Sha256::new();
        hash.update(self.mime.as_deref().unwrap_or("").as_bytes());
        hash.update([0]);
        hash.update(&self.bytes);
        format!("{:x}", hash.finalize())
    }
}
fn normalize_clipboard_image(mime: &str, bytes: Vec<u8>) -> Result<Vec<u8>, String> {
    use std::io::Cursor;
    if bytes.len() > IMAGE_CLIPBOARD_MAX {
        return Err("Clipboard image exceeds 32 MiB".into());
    }
    let format = if mime == "image/png" {
        image::ImageFormat::Png
    } else {
        image::ImageFormat::Jpeg
    };
    let mut reader = image::ImageReader::with_format(Cursor::new(&bytes), format);
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(8192);
    limits.max_image_height = Some(8192);
    limits.max_alloc = Some(64 * 1024 * 1024);
    reader.limits(limits);
    let image = reader
        .decode()
        .map_err(|e| format!("Invalid or oversized clipboard image: {e}"))?;
    if format == image::ImageFormat::Png {
        return Ok(bytes);
    }
    let mut out = Cursor::new(Vec::new());
    image
        .write_to(&mut out, image::ImageFormat::Png)
        .map_err(|e| e.to_string())?;
    let bytes = out.into_inner();
    if bytes.len() > IMAGE_CLIPBOARD_MAX {
        return Err("Converted image exceeds 32 MiB".into());
    }
    Ok(bytes)
}

/// Foreground worker. Reuses the existing startup/conflict/reconnect state by
/// comparing typed content identities; binary payloads stay out of that state.
#[cfg(target_os = "android")]
pub fn run_typed_sync(
    android: AndroidClipboard,
    host: String,
    port: u16,
    token: String,
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
    state: std::sync::Arc<std::sync::Mutex<windowlink_shared::ClipboardSyncState>>,
    mut status: impl FnMut(String),
) {
    use std::{sync::atomic::Ordering, thread, time::Duration};
    use windowlink_shared::ClipboardSyncAction;
    let mut remote_descriptor = None::<serde_json::Value>;
    let mut etag = None::<String>;
    let mut last_status = String::new();
    while !stop.load(Ordering::SeqCst) {
        // Android withholds clipboard reads in the background. Preserve the
        // sync baseline until focused instead of treating withheld data as empty.
        if !android.has_focus().unwrap_or(false) {
            thread::sleep(Duration::from_millis(200));
            continue;
        }
        let (text_enabled, image_enabled) = match state.lock() {
            Ok(state) => (state.enabled("text/plain"), state.enabled("image/png")),
            Err(_) => break,
        };
        if !text_enabled && !image_enabled {
            thread::sleep(Duration::from_millis(200));
            continue;
        }
        let result = (|| -> Result<(), String> {
            let extra = etag
                .as_deref()
                .map(|tag| vec![("If-None-Match", tag)])
                .unwrap_or_default();
            let response = crate::window_picker::request_with_limit(
                &host,
                port,
                &token,
                "GET",
                "/clipboard/descriptor",
                "",
                &[],
                64 * 1024,
                &extra,
            )?;
            if stop.load(Ordering::SeqCst) {
                return Ok(());
            }
            if response.status == 200 {
                etag = response.header("etag").map(str::to_owned);
                remote_descriptor =
                    Some(serde_json::from_slice(&response.body).map_err(|e| e.to_string())?);
            } else if response.status != 304 {
                return Err(format!("Clipboard descriptor HTTP {}", response.status));
            }
            let remote = remote_descriptor
                .as_ref()
                .ok_or_else(|| "Clipboard descriptor unavailable".to_owned())?;
            let remote_id = remote["content_id"]
                .as_str()
                .ok_or_else(|| "Clipboard content identity missing".to_owned())?;
            let local = android.typed_payload(text_enabled, image_enabled)?;
            if stop.load(Ordering::SeqCst) {
                return Ok(());
            }
            if !android.has_focus().unwrap_or(false) {
                return Ok(());
            }
            let local_id = local.identity();
            let action = state
                .lock()
                .map_err(|_| "Clipboard state unavailable".to_owned())?
                .reconcile(&local_id, remote_id);
            match action {
                Some(ClipboardSyncAction::SendToDesktop(_)) => {
                    if let Some(mime) = local.mime.as_deref() {
                        if !state
                            .lock()
                            .map_err(|_| "Clipboard state unavailable")?
                            .enabled(mime)
                        {
                            return Ok(());
                        }
                        if stop.load(Ordering::SeqCst) {
                            return Ok(());
                        }
                        let response = crate::window_picker::request_with_limit(
                            &host,
                            port,
                            &token,
                            "POST",
                            "/clipboard/content",
                            mime,
                            &local.bytes,
                            64 * 1024,
                            &[],
                        )?;
                        if response.status != 204 {
                            return Err(format!("Clipboard write HTTP {}", response.status));
                        }
                        if stop.load(Ordering::SeqCst) {
                            return Ok(());
                        }
                        state
                            .lock()
                            .map_err(|_| "Clipboard state unavailable".to_owned())?
                            .commit(&local_id);
                        etag = None;
                        remote_descriptor = None;
                    } else {
                        // Unsupported file/empty content is observed without clearing
                        // a desktop clipboard or turning a URI into text.
                        state
                            .lock()
                            .map_err(|_| "Clipboard state unavailable".to_owned())?
                            .initialize(&local_id, remote_id);
                    }
                }
                Some(ClipboardSyncAction::SetAndroidClipboard(_)) => {
                    let mime = remote["mime"].as_str();
                    if let Some(mime) = mime {
                        if !state
                            .lock()
                            .map_err(|_| "Clipboard state unavailable")?
                            .enabled(mime)
                        {
                            state
                                .lock()
                                .map_err(|_| "Clipboard state unavailable")?
                                .initialize(&local_id, remote_id);
                            return Ok(());
                        }
                        let size = remote["byte_length"].as_u64().unwrap_or(u64::MAX);
                        let limit = if mime.starts_with("text/plain") {
                            MAX_CLIPBOARD_BYTES
                        } else {
                            IMAGE_CLIPBOARD_MAX
                        };
                        if size > limit as u64 {
                            return Err("Desktop clipboard exceeds its type size limit".into());
                        }
                        let tag = format!("\"{remote_id}\"");
                        let response = crate::window_picker::request_with_limit(
                            &host,
                            port,
                            &token,
                            "GET",
                            "/clipboard/content",
                            "",
                            &[],
                            limit,
                            &[("If-Match", &tag)],
                        )?;
                        if response.status != 200 {
                            etag = None;
                            return Err(format!("Clipboard read HTTP {}", response.status));
                        }
                        let payload = TypedPayload {
                            mime: Some(mime.to_owned()),
                            bytes: response.body,
                        };
                        if payload.identity() != remote_id {
                            return Err("Clipboard changed during content read".into());
                        }
                        if stop.load(Ordering::SeqCst) {
                            return Ok(());
                        }
                        if !android.has_focus().unwrap_or(false) {
                            return Ok(());
                        }
                        if !state
                            .lock()
                            .map_err(|_| "Clipboard state unavailable")?
                            .enabled(mime)
                        {
                            return Ok(());
                        }
                        if mime.starts_with("text/plain") {
                            android.set_text(
                                std::str::from_utf8(&payload.bytes)
                                    .map_err(|_| "Clipboard text is not UTF-8".to_owned())?,
                            )?;
                        } else if mime == "image/png" {
                            let bytes = normalize_clipboard_image(mime, payload.bytes)?;
                            if stop.load(Ordering::SeqCst) {
                                return Ok(());
                            }
                            android.set_image(&bytes)?;
                        } else {
                            return Err("Desktop clipboard type is unsupported".into());
                        }
                        if stop.load(Ordering::SeqCst) {
                            return Ok(());
                        }
                        state
                            .lock()
                            .map_err(|_| "Clipboard state unavailable".to_owned())?
                            .commit(remote_id);
                    } else {
                        state
                            .lock()
                            .map_err(|_| "Clipboard state unavailable".to_owned())?
                            .initialize(&local_id, remote_id);
                    }
                }
                None => {}
            }
            Ok(())
        })();
        if stop.load(Ordering::SeqCst) {
            break;
        }
        let current = match result {
            Ok(()) => if text_enabled && image_enabled {
                "Text and image clipboard sync on"
            } else if text_enabled {
                "Text clipboard sync on"
            } else {
                "Image clipboard sync on"
            }
            .into(),
            Err(e) => format!("Clipboard: {e}"),
        };
        if current != last_status {
            status(current.clone());
            last_status = current;
        }
        for _ in 0..20 {
            if stop.load(Ordering::SeqCst) {
                break;
            }
            thread::sleep(Duration::from_millis(100));
        }
    }
}

#[cfg(test)]
mod typed_tests {
    use super::*;
    #[test]
    fn typed_identity_matches_host_protocol() {
        let payload = TypedPayload {
            mime: Some("text/plain; charset=utf-8".into()),
            bytes: b"hello".to_vec(),
        };
        use sha2::{Digest, Sha256};
        let mut hash = Sha256::new();
        hash.update(b"text/plain; charset=utf-8\0hello");
        assert_eq!(payload.identity(), format!("{:x}", hash.finalize()));
    }
    #[test]
    fn invalid_image_is_rejected() {
        assert!(normalize_clipboard_image("image/png", vec![1, 2, 3]).is_err());
    }
}
