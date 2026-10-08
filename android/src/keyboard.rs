//! Touch friendly remote keyboard controls for the Android client.

use eframe::egui;
use ironrdp_pdu::input::fast_path::{FastPathInputEvent, KeyboardFlags};

// XT set-1 codes, shared by touch controls and native function-key events.
const FUNCTION_KEYS: [(&str, u8, egui::Key); 12] = [
    ("F1", 0x3b, egui::Key::F1),
    ("F2", 0x3c, egui::Key::F2),
    ("F3", 0x3d, egui::Key::F3),
    ("F4", 0x3e, egui::Key::F4),
    ("F5", 0x3f, egui::Key::F5),
    ("F6", 0x40, egui::Key::F6),
    ("F7", 0x41, egui::Key::F7),
    ("F8", 0x42, egui::Key::F8),
    ("F9", 0x43, egui::Key::F9),
    ("F10", 0x44, egui::Key::F10),
    ("F11", 0x57, egui::Key::F11),
    ("F12", 0x58, egui::Key::F12),
];

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Modifiers {
    ctrl: bool,
    alt: bool,
    shift: bool,
    super_key: bool,
}

impl Modifiers {
    fn is_armed(self) -> bool {
        self.ctrl || self.alt || self.shift || self.super_key
    }
}

/// State and egui panel for sending keyboard input to the remote desktop.
///
/// `show` returns input events for the caller to pass to its existing RDP
/// input sender. Text is sent immediately from egui's committed text events;
/// IME preedit text stays local until the IME commits it.
#[derive(Default)]
pub struct KeyboardControls {
    text: String,
    show_function_keys: bool,
    modifiers: Modifiers,
    ime_preedit_active: bool,
    native_keys_down: Vec<egui::Key>,
    swallowed_native_key_releases: Vec<egui::Key>,
    raw_keys_down: Vec<(u8, bool)>,
}

impl KeyboardControls {
    /// Clear any armed one-shot modifiers, such as when closing the panel.
    pub fn reset(&mut self) -> Vec<FastPathInputEvent> {
        let mut releases: Vec<_> = self
            .native_keys_down
            .drain(..)
            .rev()
            .filter_map(|key| native_scan_code(key))
            .map(|(code, extended)| scan_event(code, extended, true))
            .collect();
        releases.extend(
            self.raw_keys_down
                .drain(..)
                .rev()
                .map(|(code, extended)| scan_event(code, extended, true)),
        );
        self.modifiers = Modifiers::default();
        self.text.clear();
        self.ime_preedit_active = false;
        self.swallowed_native_key_releases.clear();
        releases
    }

    /// Forward hardware independently of the phone keyboard panel. Call once
    /// from the app event path while the live desktop owns keyboard focus.
    /// Forwarded events are removed so TextEdit cannot send hardware text again.
    /// Physical keys use XT set-1 positions; the host keyboard layout determines
    /// resulting characters. IME commits retain their Unicode text behavior.
    pub fn handle_hardware_events(
        &mut self,
        incoming: &mut Vec<egui::Event>,
        remote_focus: bool,
    ) -> Vec<FastPathInputEvent> {
        if !remote_focus {
            return self.reset();
        }
        let hardware_printable = incoming.iter().any(|event| {
            matches!(event,
            egui::Event::Key { key, physical_key: Some(physical), pressed: true, .. }
                if !is_native_modifier(*key) && printable_key(*physical))
        });
        let mut outgoing = Vec::new();
        incoming.retain(|event| match event {
            egui::Event::Key {
                key,
                physical_key,
                pressed,
                ..
            } => {
                let identity = physical_key.unwrap_or(*key);
                let hardware = physical_key.is_some()
                    || is_native_modifier(identity)
                    || self.native_keys_down.contains(&identity);
                if !hardware {
                    return true;
                }
                let Some((code, extended)) = native_scan_code(identity) else {
                    return true;
                };
                if *pressed {
                    outgoing.push(scan_event(code, extended, false));
                    if !self.native_keys_down.contains(&identity) {
                        self.native_keys_down.push(identity);
                    }
                } else if remove_key(&mut self.native_keys_down, identity) {
                    outgoing.push(scan_event(code, extended, true));
                }
                false
            }
            egui::Event::Text(_) if hardware_printable => false,
            _ => true,
        });
        outgoing
    }

    /// NativeActivity key/device capture can preserve numpad and lock-key
    /// identity that egui collapses. Supply the XT code, including right-side
    /// extended modifiers; repeats are additional downs and releases are exact.
    pub fn handle_hardware_scan_code(
        &mut self,
        code: u8,
        extended: bool,
        pressed: bool,
    ) -> Vec<FastPathInputEvent> {
        let identity = (code, extended);
        if pressed {
            if !self.raw_keys_down.contains(&identity) {
                self.raw_keys_down.push(identity);
            }
            vec![scan_event(code, extended, false)]
        } else if let Some(index) = self.raw_keys_down.iter().position(|key| *key == identity) {
            self.raw_keys_down.remove(index);
            vec![scan_event(code, extended, true)]
        } else {
            Vec::new()
        }
    }

    pub fn typing_has_focus(&self, ctx: &egui::Context) -> bool {
        ctx.memory(|memory| memory.has_focus(egui::Id::new("remote-phone-typing")))
    }

    pub fn show(&mut self, ui: &mut egui::Ui) -> Vec<FastPathInputEvent> {
        let landscape = ui.ctx().content_rect().width() > ui.ctx().content_rect().height();
        if landscape {
            ui.horizontal(|ui| self.show_contents(ui, true)).inner
        } else {
            self.show_contents(ui, false)
        }
    }

    fn show_contents(&mut self, ui: &mut egui::Ui, compact: bool) -> Vec<FastPathInputEvent> {
        let mut events = Vec::new();

        // Capture committed input events before TextEdit consumes them. IME
        // preedit is intentionally ignored: it is only a candidate string and
        // may be replaced repeatedly before the user commits it.
        let incoming = ui.input(|input| input.events.clone());
        let response = ui.add_sized(
            [
                if compact {
                    180.0_f32.min(ui.available_width())
                } else {
                    ui.available_width()
                },
                48.0,
            ],
            egui::TextEdit::singleline(&mut self.text)
                .id(egui::Id::new("remote-phone-typing"))
                .hint_text("Tap to type with the phone keyboard"),
        );
        if response.has_focus() {
            ui.ctx().output_mut(|output| {
                if let Some(ime) = output.ime.as_mut() {
                    ime.purpose = egui::IMEPurpose::Terminal;
                }
            });
        }
        let has_enter_key = incoming.iter().any(|event| {
            matches!(
                event,
                egui::Event::Key {
                    key: egui::Key::Enter,
                    pressed: true,
                    ..
                }
            )
        });
        let has_special_key = incoming.iter().any(|event| {
            matches!(event, egui::Event::Key { key, pressed: true, .. } if native_scan_code(*key).is_some())
        });
        let has_newline_commit = committed_text(&incoming)
            .iter()
            .any(|text| text == "\n" || text == "\r");
        let remote_focus = response.has_focus()
            || (response.lost_focus() && (has_special_key || has_enter_key || has_newline_commit));
        if remote_focus {
            let committed = committed_text(&incoming);
            for text in &committed {
                events.extend(self.send_committed_text(text));
            }
            events.extend(self.native_key_events(&incoming, true, has_newline_commit));
            events.extend(self.send_ime_deletion(&incoming));
            if !committed.is_empty() {
                // TextEdit needs a local buffer for native IME composition.
                // Once committed text is forwarded, consume it so a later
                // frame cannot resend the accumulated contents.
                self.text.clear();
            }
        } else {
            // Keep releasing keys that were forwarded before the editor lost
            // focus; ignore new key presses outside the text field.
            events.extend(self.native_key_events(&incoming, false, false));
        }
        self.update_ime_preedit(&incoming);

        egui::ScrollArea::horizontal()
            .max_height(50.0)
            .auto_shrink([false, false])
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    modifier_toggle(ui, "F keys", &mut self.show_function_keys);
                    modifier_toggle(ui, "Ctrl", &mut self.modifiers.ctrl);
                    modifier_toggle(ui, "Alt", &mut self.modifiers.alt);
                    modifier_toggle(ui, "Shift", &mut self.modifiers.shift);
                    modifier_toggle(ui, "Super", &mut self.modifiers.super_key);

                    if self.show_function_keys {
                        for (label, code, _) in FUNCTION_KEYS {
                            if key_button(ui, label) {
                                events.extend(self.press_key(code, false));
                            }
                        }
                        return;
                    }
                    for (label, code, extended) in [
                        ("Esc", 0x01, false),
                        ("Tab", 0x0f, false),
                        ("Enter", 0x1c, false),
                        ("Backspace", 0x0e, false),
                        ("Insert", 0x52, true),
                        ("Delete", 0x53, true),
                        ("Left", 0x4b, true),
                        ("Up", 0x48, true),
                        ("Down", 0x50, true),
                        ("Right", 0x4d, true),
                        ("Home", 0x47, true),
                        ("End", 0x4f, true),
                        ("PageUp", 0x49, true),
                        ("PageDown", 0x51, true),
                        ("Tap Super", 0x5b, true),
                        ("Menu", 0x5d, true),
                        ("Caps Lock", 0x3a, false),
                        ("Num Lock", 0x45, false),
                        ("Scroll Lock", 0x46, false),
                        ("Print Screen", 0x37, true),
                        ("Pause", 0x46, true),
                    ] {
                        if key_button(ui, label) {
                            events.extend(self.press_key(code, extended));
                        }
                    }
                    if paste_button(ui, true).clicked() {
                        events.extend(self.shortcut(0x2f));
                    }
                    for (label, code) in [("Copy", 0x2e), ("Select all", 0x1e)] {
                        if key_button(ui, label) {
                            events.extend(self.shortcut(code));
                        }
                    }
                });
            });

        events
    }

    fn press_key(&mut self, code: u8, extended: bool) -> Vec<FastPathInputEvent> {
        let mut active = self.take_modifiers();
        if code == 0x5b && extended {
            active.super_key = false;
        }
        self.touch_sequence(active, code, extended)
    }

    fn shortcut(&mut self, code: u8) -> Vec<FastPathInputEvent> {
        // Shortcut buttons always include Ctrl; selected one-shot modifiers
        // are also honored and cleared after this key sequence.
        let mut active = self.take_modifiers();
        active.ctrl = true;
        self.touch_sequence(active, code, false)
    }

    fn touch_sequence(
        &self,
        mut modifiers: Modifiers,
        code: u8,
        extended: bool,
    ) -> Vec<FastPathInputEvent> {
        let held: Vec<_> = self
            .native_keys_down
            .iter()
            .filter_map(|key| native_scan_code(*key))
            .chain(self.raw_keys_down.iter().copied())
            .collect();
        modifiers.ctrl &= !held.iter().any(|(code, _)| *code == 0x1d);
        modifiers.alt &= !held.iter().any(|(code, _)| *code == 0x38);
        modifiers.shift &= !held.iter().any(|(code, _)| matches!(code, 0x2a | 0x36));
        modifiers.super_key &= !held
            .iter()
            .any(|(code, ext)| *ext && matches!(code, 0x5b | 0x5c));
        // Tapping a control for an already-held key must preserve its lifetime.
        if held.contains(&(code, extended)) {
            return Vec::new();
        }
        key_sequence(modifiers, code, extended)
    }

    fn take_modifiers(&mut self) -> Modifiers {
        std::mem::take(&mut self.modifiers)
    }

    fn send_committed_text(&mut self, text: &str) -> Vec<FastPathInputEvent> {
        if text.is_empty() {
            return Vec::new();
        }
        if text == "\n" || text == "\r" {
            return self.press_key(0x1c, false);
        }
        if !self.modifiers.is_armed() {
            return unicode_events(text);
        }
        let ascii = if text.chars().count() == 1 {
            text.chars().next().and_then(ascii_key)
        } else {
            None
        };
        let mut active = self.take_modifiers();
        if let Some((code, needs_shift)) = ascii {
            active.shift |= needs_shift;
            self.touch_sequence(active, code, false)
        } else {
            unicode_events(text)
        }
    }

    fn send_ime_deletion(&mut self, input_events: &[egui::Event]) -> Vec<FastPathInputEvent> {
        if self.ime_preedit_active || has_active_preedit(input_events) {
            return Vec::new();
        }
        let mut events = Vec::new();
        for input_event in input_events {
            if let egui::Event::Ime(egui::ImeEvent::DeleteSurrounding {
                before_chars,
                after_chars,
            }) = input_event
            {
                for _ in 0..*before_chars {
                    events.extend(self.press_key(0x0e, false));
                }
                for _ in 0..*after_chars {
                    events.extend(self.press_key(0x53, true));
                }
            }
        }
        events
    }

    fn native_key_events(
        &mut self,
        input_events: &[egui::Event],
        remote_focus: bool,
        has_newline_commit: bool,
    ) -> Vec<FastPathInputEvent> {
        let has_ime_delete = input_events.iter().any(|event| {
            matches!(
                event,
                egui::Event::Ime(egui::ImeEvent::DeleteSurrounding { .. })
            )
        });
        let composing = self.ime_preedit_active || has_active_preedit(input_events);
        let mut events = Vec::new();

        for event in input_events {
            let egui::Event::Key { key, pressed, .. } = event else {
                continue;
            };

            if !pressed {
                if remove_key(&mut self.swallowed_native_key_releases, *key) {
                    continue;
                }
                if remove_key(&mut self.native_keys_down, *key) {
                    if let Some((code, extended)) = native_scan_code(*key) {
                        events.push(scan_event(code, extended, true));
                    }
                }
                continue;
            }

            if printable_key(*key) {
                continue;
            }
            let Some((code, extended)) = native_scan_code(*key) else {
                continue;
            };

            if self.swallowed_native_key_releases.contains(key) {
                continue;
            }
            if !remote_focus {
                continue;
            }

            let ime_deletes_same_key =
                matches!(key, egui::Key::Backspace | egui::Key::Delete) && has_ime_delete;
            let composing_edit =
                matches!(key, egui::Key::Backspace | egui::Key::Delete) && composing;
            let enter_commit_duplicate = *key == egui::Key::Enter && has_newline_commit;
            if ime_deletes_same_key || composing_edit || enter_commit_duplicate {
                self.swallowed_native_key_releases.push(*key);
                continue;
            }

            if self.modifiers.is_armed() && !is_native_modifier(*key) {
                events.extend(self.press_key(code, extended));
                self.swallowed_native_key_releases.push(*key);
            } else {
                events.push(scan_event(code, extended, false));
                if !self.native_keys_down.contains(key) {
                    self.native_keys_down.push(*key);
                }
            }
        }
        events
    }

    fn update_ime_preedit(&mut self, input_events: &[egui::Event]) {
        for input_event in input_events {
            match input_event {
                egui::Event::Ime(egui::ImeEvent::Preedit { text, .. }) => {
                    self.ime_preedit_active = !text.is_empty();
                }
                egui::Event::Ime(egui::ImeEvent::Commit(_)) => {
                    self.ime_preedit_active = false;
                }
                _ => {}
            }
        }
    }
}

/// Return text the user committed, excluding composition updates. Some input
/// integrations report the same committed value through both `Text` and
/// `Ime::Commit`, so discard an exact duplicate instead of sending it twice.
fn committed_text(input_events: &[egui::Event]) -> Vec<String> {
    let mut duplicate_texts: Vec<(String, usize)> = Vec::new();
    for text in input_events.iter().filter_map(|event| match event {
        egui::Event::Ime(egui::ImeEvent::Commit(text)) if !text.is_empty() => Some(text),
        _ => None,
    }) {
        if let Some((_, count)) = duplicate_texts
            .iter_mut()
            .find(|(committed, _)| committed == text)
        {
            *count += 1;
        } else {
            duplicate_texts.push((text.clone(), 1));
        }
    }
    let has_active_preedit = has_active_preedit(input_events);

    input_events
        .iter()
        .filter_map(|event| match event {
            egui::Event::Ime(egui::ImeEvent::Commit(text)) if !text.is_empty() => {
                Some(text.clone())
            }
            egui::Event::Text(text) if !text.is_empty() && !has_active_preedit => {
                if let Some(index) = duplicate_texts
                    .iter()
                    .position(|(committed, count)| *count > 0 && committed == text)
                {
                    duplicate_texts[index].1 -= 1;
                    None
                } else {
                    Some(text.clone())
                }
            }
            _ => None,
        })
        .collect()
}

fn has_active_preedit(input_events: &[egui::Event]) -> bool {
    input_events.iter().any(|event| {
        matches!(
            event,
            egui::Event::Ime(egui::ImeEvent::Preedit { text, .. }) if !text.is_empty()
        )
    })
}

fn remove_key(keys: &mut Vec<egui::Key>, key: egui::Key) -> bool {
    if let Some(index) = keys.iter().position(|pressed| *pressed == key) {
        keys.remove(index);
        true
    } else {
        false
    }
}

fn native_scan_code(key: egui::Key) -> Option<(u8, bool)> {
    if let Some((_, code, _)) = FUNCTION_KEYS
        .iter()
        .find(|(_, _, candidate)| *candidate == key)
    {
        return Some((*code, false));
    }
    Some(match key {
        egui::Key::A => (0x1e, false),
        egui::Key::B => (0x30, false),
        egui::Key::C => (0x2e, false),
        egui::Key::D => (0x20, false),
        egui::Key::E => (0x12, false),
        egui::Key::F => (0x21, false),
        egui::Key::G => (0x22, false),
        egui::Key::H => (0x23, false),
        egui::Key::I => (0x17, false),
        egui::Key::J => (0x24, false),
        egui::Key::K => (0x25, false),
        egui::Key::L => (0x26, false),
        egui::Key::M => (0x32, false),
        egui::Key::N => (0x31, false),
        egui::Key::O => (0x18, false),
        egui::Key::P => (0x19, false),
        egui::Key::Q => (0x10, false),
        egui::Key::R => (0x13, false),
        egui::Key::S => (0x1f, false),
        egui::Key::T => (0x14, false),
        egui::Key::U => (0x16, false),
        egui::Key::V => (0x2f, false),
        egui::Key::W => (0x11, false),
        egui::Key::X => (0x2d, false),
        egui::Key::Y => (0x15, false),
        egui::Key::Z => (0x2c, false),
        egui::Key::Num1 => (0x02, false),
        egui::Key::Num2 => (0x03, false),
        egui::Key::Num3 => (0x04, false),
        egui::Key::Num4 => (0x05, false),
        egui::Key::Num5 => (0x06, false),
        egui::Key::Num6 => (0x07, false),
        egui::Key::Num7 => (0x08, false),
        egui::Key::Num8 => (0x09, false),
        egui::Key::Num9 => (0x0a, false),
        egui::Key::Num0 => (0x0b, false),
        egui::Key::Space => (0x39, false),
        egui::Key::Minus => (0x0c, false),
        egui::Key::Equals => (0x0d, false),
        egui::Key::OpenBracket => (0x1a, false),
        egui::Key::CloseBracket => (0x1b, false),
        egui::Key::Semicolon => (0x27, false),
        egui::Key::Quote => (0x28, false),
        egui::Key::Backtick => (0x29, false),
        egui::Key::Backslash => (0x2b, false),
        egui::Key::IntlBackslash => (0x56, false),
        egui::Key::Comma => (0x33, false),
        egui::Key::Period => (0x34, false),
        egui::Key::Slash => (0x35, false),
        egui::Key::ArrowDown => (0x50, true),
        egui::Key::ArrowLeft => (0x4b, true),
        egui::Key::ArrowRight => (0x4d, true),
        egui::Key::ArrowUp => (0x48, true),
        egui::Key::Escape => (0x01, false),
        egui::Key::Tab => (0x0f, false),
        egui::Key::Backspace => (0x0e, false),
        egui::Key::Enter => (0x1c, false),
        egui::Key::Insert => (0x52, true),
        egui::Key::Delete => (0x53, true),
        egui::Key::Home => (0x47, true),
        egui::Key::End => (0x4f, true),
        egui::Key::PageUp => (0x49, true),
        egui::Key::PageDown => (0x51, true),
        egui::Key::ShiftLeft => (0x2a, false),
        egui::Key::ShiftRight => (0x36, false),
        egui::Key::ControlLeft => (0x1d, false),
        egui::Key::ControlRight => (0x1d, true),
        egui::Key::AltLeft => (0x38, false),
        egui::Key::AltRight => (0x38, true),
        egui::Key::SuperLeft => (0x5b, true),
        egui::Key::SuperRight => (0x5c, true),
        _ => return None,
    })
}

fn printable_key(key: egui::Key) -> bool {
    native_scan_code(key).is_some_and(|(code, extended)| {
        !extended && matches!(code, 0x02..=0x0d | 0x10..=0x1b | 0x1e..=0x29 | 0x2b..=0x35 | 0x39)
    })
}

fn is_native_modifier(key: egui::Key) -> bool {
    matches!(
        key,
        egui::Key::ShiftLeft
            | egui::Key::ShiftRight
            | egui::Key::ControlLeft
            | egui::Key::ControlRight
            | egui::Key::AltLeft
            | egui::Key::AltRight
            | egui::Key::SuperLeft
            | egui::Key::SuperRight
    )
}

/// Vector clipboard icon stays legible with every theme and font.
pub(crate) fn paste_button(ui: &mut egui::Ui, enabled: bool) -> egui::Response {
    crate::icons::button(ui, crate::icons::Icon::Paste, "Paste", enabled, false)
}

fn modifier_toggle(ui: &mut egui::Ui, label: &str, value: &mut bool) {
    let width = button_width(ui, label);
    let response = ui.add_sized([width, 48.0], egui::Button::new(label).selected(*value));
    if response.clicked() {
        *value = !*value;
    }
}

fn key_button(ui: &mut egui::Ui, label: &str) -> bool {
    let width = button_width(ui, label);
    ui.add_sized([width, 48.0], egui::Button::new(label))
        .clicked()
}

fn button_width(ui: &egui::Ui, label: &str) -> f32 {
    let font = ui.style().text_styles[&egui::TextStyle::Button].clone();
    let text_width = ui
        .painter()
        .layout_no_wrap(label.to_owned(), font, egui::Color32::WHITE)
        .size()
        .x;
    (text_width + ui.spacing().button_padding.x * 2.0).max(48.0)
}

fn key_sequence(modifiers: Modifiers, code: u8, extended: bool) -> Vec<FastPathInputEvent> {
    let mut keys = Vec::with_capacity(10);
    if modifiers.ctrl {
        keys.push((0x1d, false));
    }
    if modifiers.alt {
        keys.push((0x38, false));
    }
    if modifiers.shift {
        keys.push((0x2a, false));
    }
    if modifiers.super_key {
        keys.push((0x5b, true));
    }

    let mut events = Vec::with_capacity(keys.len() * 2 + 2);
    for &(modifier, ext) in &keys {
        events.push(scan_event(modifier, ext, false));
    }
    events.push(scan_event(code, extended, false));
    events.push(scan_event(code, extended, true));
    for &(modifier, ext) in keys.iter().rev() {
        events.push(scan_event(modifier, ext, true));
    }
    events
}

/// US keyboard scan code for a single ASCII letter or digit. Letter case is
/// represented by Shift; digits use their unshifted scan codes.
fn ascii_key(ch: char) -> Option<(u8, bool)> {
    let lower = ch.to_ascii_lowercase();
    let code = match lower {
        'a' => 0x1e,
        'b' => 0x30,
        'c' => 0x2e,
        'd' => 0x20,
        'e' => 0x12,
        'f' => 0x21,
        'g' => 0x22,
        'h' => 0x23,
        'i' => 0x17,
        'j' => 0x24,
        'k' => 0x25,
        'l' => 0x26,
        'm' => 0x32,
        'n' => 0x31,
        'o' => 0x18,
        'p' => 0x19,
        'q' => 0x10,
        'r' => 0x13,
        's' => 0x1f,
        't' => 0x14,
        'u' => 0x16,
        'v' => 0x2f,
        'w' => 0x11,
        'x' => 0x2d,
        'y' => 0x15,
        'z' => 0x2c,
        '1' => 0x02,
        '2' => 0x03,
        '3' => 0x04,
        '4' => 0x05,
        '5' => 0x06,
        '6' => 0x07,
        '7' => 0x08,
        '8' => 0x09,
        '9' => 0x0a,
        '0' => 0x0b,
        _ => return None,
    };
    Some((code, ch.is_ascii_uppercase()))
}

fn scan_event(code: u8, extended: bool, release: bool) -> FastPathInputEvent {
    let mut flags = KeyboardFlags::empty();
    if extended {
        flags |= KeyboardFlags::EXTENDED;
    }
    if release {
        flags |= KeyboardFlags::RELEASE;
    }
    FastPathInputEvent::KeyboardEvent(flags, code)
}

fn unicode_events(text: &str) -> Vec<FastPathInputEvent> {
    text.encode_utf16()
        .flat_map(|unit| {
            [
                FastPathInputEvent::UnicodeKeyboardEvent(KeyboardFlags::empty(), unit),
                FastPathInputEvent::UnicodeKeyboardEvent(KeyboardFlags::RELEASE, unit),
            ]
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hardware(key: egui::Key, pressed: bool, repeat: bool) -> egui::Event {
        egui::Event::Key {
            key,
            physical_key: Some(key),
            pressed,
            repeat,
            modifiers: egui::Modifiers::default(),
        }
    }
    #[test]
    fn hardware_hidden_panel_forwards_physical_keys_repeats_and_consumes_text() {
        let mut keyboard = KeyboardControls::default();
        let mut incoming = vec![
            hardware(egui::Key::A, true, false),
            egui::Event::Text("a".into()),
            hardware(egui::Key::A, true, true),
            hardware(egui::Key::A, false, false),
        ];
        assert_eq!(
            keyboard.handle_hardware_events(&mut incoming, true),
            vec![
                scan_event(0x1e, false, false),
                scan_event(0x1e, false, false),
                scan_event(0x1e, false, true)
            ]
        );
        assert!(incoming.is_empty());
        assert!(keyboard.reset().is_empty());
    }
    #[test]
    fn touch_shortcut_preserves_hardware_ctrl_until_real_release() {
        let mut keyboard = KeyboardControls::default();
        let mut incoming = vec![hardware(egui::Key::ControlLeft, true, false)];
        assert_eq!(
            keyboard.handle_hardware_events(&mut incoming, true),
            vec![scan_event(0x1d, false, false)]
        );
        assert_eq!(
            keyboard.shortcut(0x2e),
            vec![
                scan_event(0x2e, false, false),
                scan_event(0x2e, false, true)
            ]
        );
        let mut incoming = vec![hardware(egui::Key::ControlLeft, false, false)];
        assert_eq!(
            keyboard.handle_hardware_events(&mut incoming, true),
            vec![scan_event(0x1d, false, true)]
        );
    }
    #[test]
    fn losing_remote_focus_releases_keys_without_forwarding_local_edit() {
        let mut keyboard = KeyboardControls::default();
        let mut input = vec![
            hardware(egui::Key::ControlRight, true, false),
            hardware(egui::Key::L, true, false),
        ];
        keyboard.handle_hardware_events(&mut input, true);
        let mut local = vec![
            hardware(egui::Key::A, true, false),
            egui::Event::Text("a".into()),
        ];
        assert_eq!(
            keyboard.handle_hardware_events(&mut local, false),
            vec![scan_event(0x26, false, true), scan_event(0x1d, true, true)]
        );
        assert_eq!(local.len(), 2);
    }
    #[test]
    fn raw_numpad_releases_and_touch_modifier_preserve_native_holds() {
        let mut keyboard = KeyboardControls::default();
        keyboard.handle_hardware_scan_code(0x36, false, true);
        keyboard.handle_hardware_scan_code(0x1c, true, true);
        keyboard.modifiers.shift = true;
        assert_eq!(
            keyboard.press_key(0x2e, false),
            vec![
                scan_event(0x2e, false, false),
                scan_event(0x2e, false, true)
            ]
        );
        assert_eq!(
            keyboard.reset(),
            vec![scan_event(0x1c, true, true), scan_event(0x36, false, true)]
        );
    }
    #[test]
    fn extended_navigation_and_delete_release_with_extended_flag() {
        assert_eq!(
            key_sequence(Modifiers::default(), 0x53, true),
            vec![scan_event(0x53, true, false), scan_event(0x53, true, true),]
        );
    }

    #[test]
    fn selected_modifiers_are_pressed_and_released_around_key() {
        let events = key_sequence(
            Modifiers {
                ctrl: true,
                alt: false,
                shift: true,
                ..Modifiers::default()
            },
            0x2e,
            false,
        );
        assert_eq!(
            events,
            vec![
                scan_event(0x1d, false, false),
                scan_event(0x2a, false, false),
                scan_event(0x2e, false, false),
                scan_event(0x2e, false, true),
                scan_event(0x2a, false, true),
                scan_event(0x1d, false, true),
            ]
        );
    }

    #[test]
    fn text_emits_unicode_press_and_release_for_each_utf16_unit() {
        assert_eq!(
            unicode_events("é"),
            vec![
                FastPathInputEvent::UnicodeKeyboardEvent(KeyboardFlags::empty(), 0x00e9),
                FastPathInputEvent::UnicodeKeyboardEvent(KeyboardFlags::RELEASE, 0x00e9),
            ]
        );
    }

    #[test]
    fn ctrl_l_commit_is_a_scan_code_shortcut_and_consumes_modifiers() {
        let mut keyboard = KeyboardControls {
            modifiers: Modifiers {
                ctrl: true,
                ..Modifiers::default()
            },
            ..KeyboardControls::default()
        };
        assert_eq!(ascii_key('L'), Some((0x26, true)));
        assert_eq!(
            keyboard.send_committed_text("L"),
            vec![
                scan_event(0x1d, false, false),
                scan_event(0x2a, false, false),
                scan_event(0x26, false, false),
                scan_event(0x26, false, true),
                scan_event(0x2a, false, true),
                scan_event(0x1d, false, true),
            ]
        );
        assert_eq!(keyboard.modifiers, Modifiers::default());
    }

    #[test]
    fn preedit_is_not_sent_and_ime_commit_is_sent_once() {
        let preedit = [
            egui::Event::Ime(egui::ImeEvent::Preedit {
                text: "に".into(),
                active_range_chars: None,
            }),
            egui::Event::Text("に".into()),
        ];
        assert!(committed_text(&preedit).is_empty());

        let commit = [
            egui::Event::Ime(egui::ImeEvent::Commit("日本".into())),
            egui::Event::Text("日本".into()),
        ];
        assert_eq!(committed_text(&commit), vec!["日本"]);
    }

    #[test]
    fn committed_plain_text_is_kept_in_event_order() {
        let input = [
            egui::Event::Text("A".into()),
            egui::Event::Text("🙂".into()),
        ];
        assert_eq!(committed_text(&input), vec!["A", "🙂"]);

        let duplicate = [
            egui::Event::Ime(egui::ImeEvent::Commit("a".into())),
            egui::Event::Text("a".into()),
            egui::Event::Text("a".into()),
        ];
        assert_eq!(committed_text(&duplicate), vec!["a", "a"]);
    }

    #[test]
    fn native_navigation_preserves_key_down_and_up() {
        let mut keyboard = KeyboardControls::default();
        let down = [egui::Event::Key {
            key: egui::Key::ArrowLeft,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: egui::Modifiers::default(),
        }];
        assert_eq!(
            keyboard.native_key_events(&down, true, false),
            vec![scan_event(0x4b, true, false)]
        );

        let up = [egui::Event::Key {
            key: egui::Key::ArrowLeft,
            physical_key: None,
            pressed: false,
            repeat: false,
            modifiers: egui::Modifiers::default(),
        }];
        assert_eq!(
            keyboard.native_key_events(&up, false, false),
            vec![scan_event(0x4b, true, true)]
        );
    }

    #[test]
    fn native_clipboard_semantics_are_not_sent_as_bare_letters() {
        assert_eq!(native_scan_code(egui::Key::Copy), None);
        assert_eq!(native_scan_code(egui::Key::Cut), None);
        assert_eq!(native_scan_code(egui::Key::Paste), None);
    }

    #[test]
    fn reset_releases_all_native_keys_in_reverse_press_order() {
        let mut keyboard = KeyboardControls::default();
        let down = [
            egui::Event::Key {
                key: egui::Key::ControlLeft,
                physical_key: None,
                pressed: true,
                repeat: false,
                modifiers: egui::Modifiers::default(),
            },
            egui::Event::Key {
                key: egui::Key::ArrowLeft,
                physical_key: None,
                pressed: true,
                repeat: false,
                modifiers: egui::Modifiers::default(),
            },
        ];
        assert_eq!(
            keyboard.native_key_events(&down, true, false),
            vec![
                scan_event(0x1d, false, false),
                scan_event(0x4b, true, false)
            ]
        );
        assert_eq!(
            keyboard.reset(),
            vec![scan_event(0x4b, true, true), scan_event(0x1d, false, true)]
        );
        assert!(keyboard.native_keys_down.is_empty());
    }

    #[test]
    fn native_backspace_and_ime_delete_are_not_sent_twice() {
        let mut keyboard = KeyboardControls::default();
        let down = [
            egui::Event::Key {
                key: egui::Key::Backspace,
                physical_key: None,
                pressed: true,
                repeat: false,
                modifiers: egui::Modifiers::default(),
            },
            egui::Event::Ime(egui::ImeEvent::DeleteSurrounding {
                before_chars: 1,
                after_chars: 0,
            }),
        ];
        assert!(keyboard.native_key_events(&down, true, false).is_empty());
        assert_eq!(
            keyboard.send_ime_deletion(&down),
            key_sequence(Modifiers::default(), 0x0e, false)
        );
        let up = [egui::Event::Key {
            key: egui::Key::Backspace,
            physical_key: None,
            pressed: false,
            repeat: false,
            modifiers: egui::Modifiers::default(),
        }];
        assert!(keyboard.native_key_events(&up, false, false).is_empty());
    }

    #[test]
    fn ime_backspace_is_forwarded_but_composition_edits_stay_local() {
        let deletion = [egui::Event::Ime(egui::ImeEvent::DeleteSurrounding {
            before_chars: 1,
            after_chars: 0,
        })];
        let mut keyboard = KeyboardControls::default();
        assert_eq!(
            keyboard.send_ime_deletion(&deletion),
            key_sequence(Modifiers::default(), 0x0e, false)
        );

        let composition_edit = [
            egui::Event::Ime(egui::ImeEvent::Preedit {
                text: "に".into(),
                active_range_chars: None,
            }),
            deletion[0].clone(),
        ];
        assert!(keyboard.send_ime_deletion(&composition_edit).is_empty());

        keyboard.ime_preedit_active = true;
        assert!(keyboard.send_ime_deletion(&deletion).is_empty());
    }

    #[test]
    fn super_modifier_uses_extended_windows_key_scan_code() {
        assert_eq!(
            key_sequence(
                Modifiers {
                    super_key: true,
                    ..Modifiers::default()
                },
                0x26,
                false
            ),
            vec![
                scan_event(0x5b, true, false),
                scan_event(0x26, false, false),
                scan_event(0x26, false, true),
                scan_event(0x5b, true, true),
            ]
        );
    }
    #[test]
    fn function_key_shortcuts_release_modifiers_and_f11_f12_use_distinct_codes() {
        assert_eq!(native_scan_code(egui::Key::F1), Some((0x3b, false)));
        assert_eq!(native_scan_code(egui::Key::F10), Some((0x44, false)));
        assert_eq!(native_scan_code(egui::Key::F11), Some((0x57, false)));
        assert_eq!(native_scan_code(egui::Key::F12), Some((0x58, false)));
        let mut keyboard = KeyboardControls::default();
        keyboard.modifiers.alt = true;
        assert_eq!(
            keyboard.press_key(0x3e, false),
            vec![
                scan_event(0x38, false, false),
                scan_event(0x3e, false, false),
                scan_event(0x3e, false, true),
                scan_event(0x38, false, true),
            ]
        );
        assert_eq!(keyboard.modifiers, Modifiers::default());
    }

    #[test]
    fn standalone_super_does_not_double_press_an_armed_super_modifier() {
        let mut keyboard = KeyboardControls::default();
        keyboard.modifiers.super_key = true;
        assert_eq!(
            keyboard.press_key(0x5b, true),
            vec![scan_event(0x5b, true, false), scan_event(0x5b, true, true),]
        );
        assert_eq!(keyboard.modifiers, Modifiers::default());
    }
}
