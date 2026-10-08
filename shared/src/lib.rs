//! Small, platform-neutral coordinate helpers shared by WindowLink clients.

/// A point in normalized screen coordinates. Values are expected in `0.0..=1.0`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct NormalizedPoint {
    pub x: f64,
    pub y: f64,
}

impl NormalizedPoint {
    pub const CENTER: Self = Self { x: 0.5, y: 0.5 };

    pub const fn new(x: f64, y: f64) -> Self {
        Self { x, y }
    }

    fn clamped(self) -> Self {
        Self {
            x: unit(self.x),
            y: unit(self.y),
        }
    }
}

/// Remote desktop dimensions in pixels.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DesktopSize {
    pub width: u32,
    pub height: u32,
}

impl DesktopSize {
    pub const fn new(width: u32, height: u32) -> Self {
        Self { width, height }
    }

    /// Map normalized coordinates to inclusive RDP pixel coordinates (`0..width-1`).
    /// Returns `None` for empty dimensions.
    pub fn map(self, point: NormalizedPoint) -> Option<DesktopPoint> {
        if self.width == 0 || self.height == 0 {
            return None;
        }
        let p = point.clamped();
        Some(DesktopPoint {
            x: (p.x * (self.width - 1) as f64).round() as u32,
            y: (p.y * (self.height - 1) as f64).round() as u32,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DesktopPoint {
    pub x: u32,
    pub y: u32,
}

/// A zoomed view into the remote image. `center` is in image-normalized coordinates.
/// `zoom == 1` shows the entire desktop; larger values magnify around the view center.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Viewport {
    center: NormalizedPoint,
    zoom: f64,
}

impl Default for Viewport {
    fn default() -> Self {
        Self::new()
    }
}

impl Viewport {
    pub const MIN_ZOOM: f64 = 1.0;
    pub const MAX_ZOOM: f64 = 32.0;

    pub const fn new() -> Self {
        Self {
            center: NormalizedPoint::CENTER,
            zoom: 1.0,
        }
    }

    pub fn zoom(&self) -> f64 {
        self.zoom
    }
    pub fn center(&self) -> NormalizedPoint {
        self.center
    }
    pub fn reset(&mut self) {
        *self = Self::new();
    }

    /// Aim the view at a specific region of the desktop, e.g. one window.
    /// `center` is the region center in normalized image coordinates and `zoom`
    /// magnifies so the region fills the view.
    pub fn focus(&mut self, center: NormalizedPoint, zoom: f64) {
        self.zoom = zoom.clamp(Self::MIN_ZOOM, Self::MAX_ZOOM);
        self.center = center.clamped();
        self.clamp_center();
    }

    /// Translate the image under a drag, where `delta` is normalized screen motion.
    /// Dragging right moves the viewed image right, so the source center moves left.
    pub fn pan(&mut self, delta: NormalizedPoint) {
        // A transient invalid pointer delta must not poison the persistent viewport state.
        if !delta.x.is_finite() || !delta.y.is_finite() {
            return;
        }
        self.center.x -= delta.x / self.zoom;
        self.center.y -= delta.y / self.zoom;
        self.clamp_center();
    }

    /// Change magnification while keeping the source point beneath `anchor` fixed.
    /// `factor` is multiplicative (e.g. `1.2` zooms in by 20%).
    pub fn zoom_at(&mut self, factor: f64, anchor: NormalizedPoint) {
        if !factor.is_finite() || factor <= 0.0 {
            return;
        }
        let anchor = anchor.clamped();
        let source = self.to_source(anchor);
        self.zoom = (self.zoom * factor).clamp(Self::MIN_ZOOM, Self::MAX_ZOOM);
        self.center = NormalizedPoint {
            x: source.x - (anchor.x - 0.5) / self.zoom,
            y: source.y - (anchor.y - 0.5) / self.zoom,
        };
        self.clamp_center();
    }

    /// Convert a normalized position in the displayed view to source-image coordinates.
    pub fn to_source(&self, view_point: NormalizedPoint) -> NormalizedPoint {
        let p = view_point.clamped();
        NormalizedPoint {
            x: unit(self.center.x + (p.x - 0.5) / self.zoom),
            y: unit(self.center.y + (p.y - 0.5) / self.zoom),
        }
    }

    /// Convert a displayed-view touch directly to remote desktop pixels.
    pub fn map_touch(&self, point: NormalizedPoint, desktop: DesktopSize) -> Option<DesktopPoint> {
        desktop.map(self.to_source(point))
    }

    /// Visible source bounds when the image fills a view without stretching.
    pub fn aspect_bounds(
        &self,
        desktop: DesktopSize,
        view_aspect: f64,
    ) -> (NormalizedPoint, NormalizedPoint) {
        let span = self.aspect_span(desktop, view_aspect);
        (
            NormalizedPoint::new(self.center.x - span.x / 2.0, self.center.y - span.y / 2.0),
            NormalizedPoint::new(self.center.x + span.x / 2.0, self.center.y + span.y / 2.0),
        )
    }

    pub fn map_touch_aspect(
        &self,
        point: NormalizedPoint,
        desktop: DesktopSize,
        view_aspect: f64,
    ) -> Option<DesktopPoint> {
        let point = point.clamped();
        let (min, max) = self.aspect_bounds(desktop, view_aspect);
        desktop.map(NormalizedPoint::new(
            min.x + point.x * (max.x - min.x),
            min.y + point.y * (max.y - min.y),
        ))
    }

    pub fn pan_aspect(&mut self, delta: NormalizedPoint, desktop: DesktopSize, view_aspect: f64) {
        if !delta.x.is_finite() || !delta.y.is_finite() {
            return;
        }
        let span = self.aspect_span(desktop, view_aspect);
        self.center.x -= delta.x * span.x;
        self.center.y -= delta.y * span.y;
        self.clamp_center_to_span(span);
    }

    pub fn zoom_at_aspect(
        &mut self,
        factor: f64,
        anchor: NormalizedPoint,
        desktop: DesktopSize,
        view_aspect: f64,
    ) {
        if !factor.is_finite() || factor <= 0.0 {
            return;
        }
        let anchor = anchor.clamped();
        let old_span = self.aspect_span(desktop, view_aspect);
        let source_x = self.center.x + (anchor.x - 0.5) * old_span.x;
        let source_y = self.center.y + (anchor.y - 0.5) * old_span.y;
        self.zoom = (self.zoom * factor).clamp(Self::MIN_ZOOM, Self::MAX_ZOOM);
        let span = self.aspect_span(desktop, view_aspect);
        self.center.x = source_x - (anchor.x - 0.5) * span.x;
        self.center.y = source_y - (anchor.y - 0.5) * span.y;
        self.clamp_center_to_span(span);
    }

    fn aspect_span(&self, desktop: DesktopSize, view_aspect: f64) -> NormalizedPoint {
        if desktop.width == 0
            || desktop.height == 0
            || !view_aspect.is_finite()
            || view_aspect <= 0.0
        {
            return NormalizedPoint::new(1.0 / self.zoom, 1.0 / self.zoom);
        }
        let source_aspect = desktop.width as f64 / desktop.height as f64;
        NormalizedPoint::new(
            (view_aspect / source_aspect).min(1.0) / self.zoom,
            (source_aspect / view_aspect).min(1.0) / self.zoom,
        )
    }

    fn clamp_center(&mut self) {
        let half = 0.5 / self.zoom;
        self.center.x = self.center.x.clamp(half, 1.0 - half);
        self.center.y = self.center.y.clamp(half, 1.0 - half);
    }

    fn clamp_center_to_span(&mut self, span: NormalizedPoint) {
        self.center.x = self.center.x.clamp(span.x / 2.0, 1.0 - span.x / 2.0);
        self.center.y = self.center.y.clamp(span.y / 2.0, 1.0 - span.y / 2.0);
    }
}

fn unit(value: f64) -> f64 {
    if value.is_nan() {
        0.0
    } else {
        value.clamp(0.0, 1.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalized_map_uses_rdp_pixel_bounds() {
        let size = DesktopSize::new(1920, 1080);
        assert_eq!(
            size.map(NormalizedPoint::new(0.0, 0.0)),
            Some(DesktopPoint { x: 0, y: 0 })
        );
        assert_eq!(
            size.map(NormalizedPoint::new(1.0, 1.0)),
            Some(DesktopPoint { x: 1919, y: 1079 })
        );
        assert_eq!(
            size.map(NormalizedPoint::new(0.5, 0.5)),
            Some(DesktopPoint { x: 960, y: 540 })
        );
        assert_eq!(DesktopSize::new(0, 20).map(NormalizedPoint::CENTER), None);
    }

    #[test]
    fn input_is_clamped_even_if_touch_is_outside_bounds() {
        let size = DesktopSize::new(100, 50);
        assert_eq!(
            size.map(NormalizedPoint::new(-3.0, 4.0)),
            Some(DesktopPoint { x: 0, y: 49 })
        );
    }

    #[test]
    fn zoom_anchor_stays_on_same_source_pixel() {
        let mut view = Viewport::new();
        let anchor = NormalizedPoint::new(0.7, 0.3);
        let before = view.to_source(anchor);
        view.zoom_at(2.0, anchor);
        let after = view.to_source(anchor);
        assert!((before.x - after.x).abs() < 1e-12);
        assert!((before.y - after.y).abs() < 1e-12);
    }

    #[test]
    fn pan_and_zoom_stay_within_image() {
        let mut view = Viewport::new();
        view.zoom_at(4.0, NormalizedPoint::CENTER);
        view.pan(NormalizedPoint::new(10.0, -10.0));
        assert_eq!(view.center(), NormalizedPoint::new(0.125, 0.875));
        for p in [
            NormalizedPoint::new(0.0, 0.0),
            NormalizedPoint::new(1.0, 1.0),
        ] {
            let source = view.to_source(p);
            assert!((0.0..=1.0).contains(&source.x));
            assert!((0.0..=1.0).contains(&source.y));
        }
    }

    #[test]
    fn invalid_pan_does_not_corrupt_viewport() {
        let mut view = Viewport::new();
        view.zoom_at(2.0, NormalizedPoint::CENTER);
        let before = view;
        view.pan(NormalizedPoint::new(f64::NAN, 0.1));
        assert_eq!(view, before);
        assert_eq!(
            view.to_source(NormalizedPoint::CENTER),
            NormalizedPoint::CENTER
        );
    }

    #[test]
    fn portrait_fill_crops_width_and_maps_touches() {
        let desktop = DesktopSize::new(2000, 1000);
        let mut view = Viewport::new();
        let (min, max) = view.aspect_bounds(desktop, 0.5);
        assert_eq!(min, NormalizedPoint::new(0.375, 0.0));
        assert_eq!(max, NormalizedPoint::new(0.625, 1.0));
        assert_eq!(
            view.map_touch_aspect(NormalizedPoint::CENTER, desktop, 0.5),
            desktop.map(NormalizedPoint::CENTER)
        );
        view.pan_aspect(NormalizedPoint::new(0.5, 0.0), desktop, 0.5);
        assert_eq!(view.center(), NormalizedPoint::new(0.375, 0.5));
        view.zoom_at_aspect(2.0, NormalizedPoint::CENTER, desktop, 0.5);
        let (min, max) = view.aspect_bounds(desktop, 0.5);
        assert!((min.x - 0.3125).abs() < 1e-12);
        assert!((max.x - 0.4375).abs() < 1e-12);
    }

    #[test]
    fn zoom_limits_and_reset() {
        let mut view = Viewport::new();
        view.zoom_at(1000.0, NormalizedPoint::CENTER);
        assert_eq!(view.zoom(), Viewport::MAX_ZOOM);
        view.zoom_at(0.001, NormalizedPoint::CENTER);
        assert_eq!(view.zoom(), Viewport::MIN_ZOOM);
        view.pan(NormalizedPoint::new(0.3, 0.0));
        view.reset();
        assert_eq!(view, Viewport::new());
    }
}

/// Maximum UTF-8 clipboard payload accepted by the control API (1 MiB).
pub const MAX_CLIPBOARD_BYTES: usize = 1024 * 1024;
/// Maximum individual file accepted by the control API (100 MiB).
pub const MAX_FILE_BYTES: u64 = 100 * 1024 * 1024;

/// Connection identity for a clipboard baseline. The bearer token participates in equality
/// but is intentionally omitted from Debug output to avoid leaking the secret.
#[derive(Clone, PartialEq, Eq)]
pub struct ClipboardSyncIdentity {
    rdp_host: String,
    rdp_port: u16,
    control_host: String,
    control_port: u16,
    username: String,
    bearer_token: String,
}

impl ClipboardSyncIdentity {
    pub fn new(
        rdp_host: impl Into<String>,
        rdp_port: u16,
        control_host: impl Into<String>,
        control_port: u16,
        username: impl Into<String>,
        bearer_token: impl Into<String>,
    ) -> Self {
        Self {
            rdp_host: rdp_host.into(),
            rdp_port,
            control_host: control_host.into(),
            control_port,
            username: username.into(),
            bearer_token: bearer_token.into(),
        }
    }
}

/// File metadata returned by `GET /files`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SharedFileInfo {
    pub name: String,
    pub size: u64,
    pub modified_unix: Option<u64>,
}

/// Check the one-segment filename contract used by the authenticated file API.
pub fn valid_shared_file_name(name: &str) -> bool {
    !name.is_empty()
        && name != "."
        && name != ".."
        && !name
            .chars()
            .any(|c| c == '/' || c == '\\' || c.is_control())
}

/// Percent-encode a valid UTF-8 filename for one URL path segment.
pub fn encode_file_path_segment(name: &str) -> Option<String> {
    if !valid_shared_file_name(name) {
        return None;
    }
    let mut encoded = String::with_capacity(name.len());
    for byte in name.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~') {
            encoded.push(byte as char);
        } else {
            encoded.push('%');
            encoded.push(char::from_digit((byte >> 4) as u32, 16)?.to_ascii_uppercase());
            encoded.push(char::from_digit((byte & 0x0f) as u32, 16)?.to_ascii_uppercase());
        }
    }
    Some(encoded)
}

#[cfg(test)]
mod file_protocol_tests {
    use super::*;

    #[test]
    fn file_names_are_single_safe_path_segments() {
        for name in [
            "",
            ".",
            "..",
            "a/b",
            "a\\b",
            "nul\0name",
            "line\nbreak",
            "tab\tname",
        ] {
            assert!(!valid_shared_file_name(name), "accepted {name:?}");
            assert_eq!(encode_file_path_segment(name), None);
        }
        for name in ["report.txt", "space name.txt", "résumé #1.txt"] {
            assert!(valid_shared_file_name(name));
            assert!(encode_file_path_segment(name).is_some());
        }
    }

    #[test]
    fn file_path_segments_escape_reserved_and_non_ascii_bytes() {
        assert_eq!(
            encode_file_path_segment("space name.txt").as_deref(),
            Some("space%20name.txt")
        );
        assert_eq!(
            encode_file_path_segment("a#b?.txt").as_deref(),
            Some("a%23b%3F.txt")
        );
        assert_eq!(
            encode_file_path_segment("café.txt").as_deref(),
            Some("caf%C3%A9.txt")
        );
    }

    #[test]
    fn payload_limits_match_control_api_contract() {
        assert_eq!(MAX_CLIPBOARD_BYTES, 1_048_576);
        assert_eq!(MAX_FILE_BYTES, 104_857_600);
    }
}

/// Text clipboard synchronization decisions for the Android app and desktop host.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClipboardSyncAction {
    /// Send this text to the desktop with `POST /clipboard`.
    SendToDesktop(String),
    /// Put this text in the Android clipboard.
    SetAndroidClipboard(String),
}

/// Tracks the last observed Android and desktop clipboard values and chooses a direction
/// without echo loops. When both sides change in one polling interval, desktop wins.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClipboardSyncState {
    text_enabled: bool,
    image_enabled: bool,
    android_baseline: Option<String>,
    desktop_baseline: Option<String>,
}

impl Default for ClipboardSyncState {
    fn default() -> Self {
        Self::new()
    }
}

impl ClipboardSyncState {
    pub fn set_enabled(&mut self, text: bool, images: bool) {
        if (text, images) != (self.text_enabled, self.image_enabled) {
            self.android_baseline = None;
            self.desktop_baseline = None;
        }
        self.text_enabled = text;
        self.image_enabled = images;
    }
    pub fn enabled(&self, mime: &str) -> bool {
        (self.text_enabled && mime.starts_with("text/plain"))
            || (self.image_enabled && matches!(mime, "image/png" | "image/jpeg"))
    }
    pub const fn new() -> Self {
        Self {
            text_enabled: true,
            image_enabled: true,
            android_baseline: None,
            desktop_baseline: None,
        }
    }

    /// Whether both sides have a baseline from initialization or a completed transfer.
    pub const fn is_initialized(&self) -> bool {
        self.android_baseline.is_some() && self.desktop_baseline.is_some()
    }

    /// Seed from both current clipboards without overwriting either side at startup.
    pub fn initialize(&mut self, android: &str, desktop: &str) -> bool {
        if android.len() > MAX_CLIPBOARD_BYTES || desktop.len() > MAX_CLIPBOARD_BYTES {
            return false;
        }
        self.android_baseline = Some(android.to_owned());
        self.desktop_baseline = Some(desktop.to_owned());
        true
    }

    /// Compare current Android and desktop values. If not initialized, records the current
    /// values and returns no action. Call `commit` only after a returned transfer succeeds.
    pub fn reconcile(&mut self, android: &str, desktop: &str) -> Option<ClipboardSyncAction> {
        if android.len() > MAX_CLIPBOARD_BYTES || desktop.len() > MAX_CLIPBOARD_BYTES {
            return None;
        }
        let (Some(android_before), Some(desktop_before)) = (
            self.android_baseline.as_deref(),
            self.desktop_baseline.as_deref(),
        ) else {
            self.initialize(android, desktop);
            return None;
        };
        if android == desktop {
            self.commit(android);
            return None;
        }
        let android_changed = android != android_before;
        let desktop_changed = desktop != desktop_before;
        match (android_changed, desktop_changed) {
            (false, false) => None,
            (true, false) => Some(ClipboardSyncAction::SendToDesktop(android.to_owned())),
            (false, true) => Some(ClipboardSyncAction::SetAndroidClipboard(desktop.to_owned())),
            // Concurrent unequal edits: prefer the desktop clipboard.
            (true, true) => Some(ClipboardSyncAction::SetAndroidClipboard(desktop.to_owned())),
        }
    }

    /// Record a value on both sides after it has been successfully copied to the other side.
    pub fn commit(&mut self, text: &str) -> bool {
        if text.len() > MAX_CLIPBOARD_BYTES {
            return false;
        }
        self.android_baseline = Some(text.to_owned());
        self.desktop_baseline = Some(text.to_owned());
        true
    }
}

#[cfg(test)]
mod clipboard_protocol_tests {
    #[test]
    fn disabling_sync_types_resets_baselines_and_preserves_defaults() {
        let mut state = super::ClipboardSyncState::default();
        assert!(state.enabled("text/plain"));
        assert!(state.enabled("image/png"));
        state.initialize("a", "b");
        state.set_enabled(true, false);
        assert!(!state.is_initialized());
        assert!(state.enabled("text/plain"));
        assert!(!state.enabled("image/png"));
        state.set_enabled(false, false);
        assert!(!state.enabled("text/plain"));
    }

    use super::*;

    #[test]
    fn initialization_does_not_overwrite_either_clipboard() {
        let mut state = ClipboardSyncState::new();
        assert!(!state.is_initialized());
        assert_eq!(state.reconcile("phone", "desktop"), None);
        assert!(state.is_initialized());
        assert_eq!(
            state.reconcile("phone", "desktop"),
            None,
            "stable initial differences are not sync changes"
        );
        assert_eq!(
            state.reconcile("phone edit", "desktop"),
            Some(ClipboardSyncAction::SendToDesktop("phone edit".into()))
        );
    }

    #[test]
    fn clipboard_identity_changes_with_host_control_port_or_secret() {
        let baseline =
            ClipboardSyncIdentity::new("host-a", 3389, "host-a", 3391, "user", "secret-a");
        assert!(
            baseline
                == ClipboardSyncIdentity::new("host-a", 3389, "host-a", 3391, "user", "secret-a")
        );
        assert!(
            baseline
                != ClipboardSyncIdentity::new("host-b", 3389, "host-b", 3391, "user", "secret-a")
        );
        assert!(
            baseline
                != ClipboardSyncIdentity::new("host-a", 3389, "host-a", 3392, "user", "secret-a")
        );
        assert!(
            baseline
                != ClipboardSyncIdentity::new("host-a", 3389, "host-a", 3391, "user", "secret-b")
        );
    }

    #[test]
    fn phone_only_change_is_sent_to_desktop_without_echo() {
        let mut state = ClipboardSyncState::default();
        assert!(state.initialize("same", "same"));
        let action = state.reconcile("phone edit", "same");
        assert_eq!(
            action,
            Some(ClipboardSyncAction::SendToDesktop("phone edit".into()))
        );
        assert!(state.commit("phone edit"));
        assert_eq!(state.reconcile("phone edit", "phone edit"), None);
    }

    #[test]
    fn sync_baseline_survives_worker_restart() {
        let mut state = ClipboardSyncState::new();
        assert!(state.initialize("phone old", "desktop old"));
        let mut restarted_worker = state.clone();
        assert_eq!(
            restarted_worker.reconcile("phone copied in Chrome", "desktop old"),
            Some(ClipboardSyncAction::SendToDesktop(
                "phone copied in Chrome".into()
            ))
        );
        assert!(restarted_worker.commit("phone copied in Chrome"));
        assert_eq!(
            restarted_worker.reconcile("phone copied in Chrome", "phone copied in Chrome"),
            None
        );
    }

    #[test]
    fn desktop_only_change_updates_phone() {
        let mut state = ClipboardSyncState::default();
        state.initialize("same", "same");
        assert_eq!(
            state.reconcile("same", "desktop edit"),
            Some(ClipboardSyncAction::SetAndroidClipboard(
                "desktop edit".into()
            ))
        );
        state.commit("desktop edit");
        assert_eq!(state.reconcile("desktop edit", "desktop edit"), None);
    }

    #[test]
    fn equal_changed_values_need_no_network_write() {
        let mut state = ClipboardSyncState::default();
        state.initialize("base", "different initial desktop value");
        assert_eq!(state.reconcile("now equal", "now equal"), None);
        assert_eq!(state.reconcile("now equal", "now equal"), None);
    }

    #[test]
    fn simultaneous_edits_choose_desktop_and_retry_until_committed() {
        let mut state = ClipboardSyncState::default();
        state.initialize("base", "base");
        let expected = Some(ClipboardSyncAction::SetAndroidClipboard(
            "desktop edit".into(),
        ));
        assert_eq!(state.reconcile("phone edit", "desktop edit"), expected);
        assert_eq!(state.reconcile("phone edit", "desktop edit"), expected);
        state.commit("desktop edit");
        assert_eq!(state.reconcile("desktop edit", "desktop edit"), None);
    }

    #[test]
    fn equal_simultaneous_edits_need_no_transfer() {
        let mut state = ClipboardSyncState::default();
        state.initialize("base", "base");
        assert_eq!(state.reconcile("same edit", "same edit"), None);
        assert_eq!(state.reconcile("same edit", "same edit"), None);
    }

    #[test]
    fn failed_transfer_is_retried_until_commit() {
        let mut state = ClipboardSyncState::default();
        state.initialize("base", "base");
        let expected = Some(ClipboardSyncAction::SendToDesktop("new".into()));
        assert_eq!(state.reconcile("new", "base"), expected);
        assert_eq!(state.reconcile("new", "base"), expected);
        state.commit("new");
        assert_eq!(state.reconcile("new", "new"), None);
    }

    #[test]
    fn oversize_clipboard_text_is_not_propagated_or_committed() {
        let too_large = "x".repeat(MAX_CLIPBOARD_BYTES + 1);
        let mut state = ClipboardSyncState::default();
        assert_eq!(state.reconcile(&too_large, "ok"), None);
        assert!(!state.initialize(&too_large, "ok"));
        assert!(!state.commit(&too_large));
        assert_eq!(state.reconcile("ok", "ok"), None);
    }
}
