//! NativeActivity hardware keys preserve numpad/lock identity before winit
//! collapses it. Java consumes accepted keys; other input follows NativeActivity.
use std::collections::VecDeque;
use std::sync::{
    Mutex,
    atomic::{AtomicBool, Ordering},
};

static REMOTE_FOCUS: AtomicBool = AtomicBool::new(false);
static STATE: Mutex<CaptureState> = Mutex::new(CaptureState {
    events: VecDeque::new(),
    held: Vec::new(),
    overflow: false,
    blocked: Vec::new(),
});
const QUEUE_LIMIT: usize = 512;

#[derive(Debug, Clone, Copy)]
pub struct HardwareKeyEvent {
    pub code: u8,
    pub extended: bool,
    pub pressed: bool,
}
struct CaptureState {
    events: VecDeque<HardwareKeyEvent>,
    held: Vec<(i32, i32)>,
    overflow: bool,
    blocked: Vec<(i32, i32)>,
}

pub fn set_remote_focus(focused: bool) {
    REMOTE_FOCUS.store(focused, Ordering::Release);
    if !focused {
        let mut state = STATE.lock().unwrap_or_else(|e| e.into_inner());
        state.blocked = state.held.clone();
    }
}
/// An overflow requires session cleanup; button/key transitions are never
/// silently discarded while continuing an active connection.
pub fn drain() -> Result<Vec<HardwareKeyEvent>, &'static str> {
    let mut state = STATE.lock().unwrap_or_else(|e| e.into_inner());
    if std::mem::take(&mut state.overflow) {
        state.events.clear();
        state.held.clear();
        state.blocked.clear();
        REMOTE_FOCUS.store(false, Ordering::Release);
        return Err("Hardware keyboard queue overflow; reconnect to clear remote input");
    }
    Ok(state.events.drain(..).collect())
}
fn capture(key: i32, device: i32, action: i32) -> bool {
    let Some((code, extended)) = android_scan_code(key) else {
        return false;
    };
    if action != 0 && action != 1 {
        return false;
    }
    let mut state = STATE.lock().unwrap_or_else(|e| e.into_inner());
    let identity = (device, key);
    let held = state.held.iter().position(|held| *held == identity);
    if action == 0 && state.blocked.contains(&identity) {
        // A lifecycle cleanup released this key remotely. Ignore repeats until
        // its real lift so a new target/session cannot inherit a stale hold.
        return true;
    }
    if action == 1 {
        state.blocked.retain(|key| *key != identity);
    }
    if (action == 0 && !REMOTE_FOCUS.load(Ordering::Acquire)) || (action == 1 && held.is_none()) {
        return false;
    }
    if action == 0 && held.is_none() {
        state.held.push(identity);
    }
    if action == 1 {
        state.held.remove(held.unwrap());
    }
    if state.events.len() >= QUEUE_LIMIT {
        state.overflow = true;
    } else {
        state.events.push_back(HardwareKeyEvent {
            code,
            extended,
            pressed: action == 0,
        });
    }
    true
}

#[cfg(target_os = "android")]
#[unsafe(no_mangle)]
pub extern "system" fn Java_dev_windowlink_client_WindowLinkActivity_nativeWindowLinkHardwareKey(
    _env: *mut std::ffi::c_void,
    _activity: *mut std::ffi::c_void,
    key: i32,
    device: i32,
    action: i32,
) -> u8 {
    u8::from(capture(key, device, action))
}

/// Android KeyEvent codes translated to XT set-1 physical positions. The host
/// keyboard layout decides letters/punctuation; IME input remains Unicode.
fn android_scan_code(key: i32) -> Option<(u8, bool)> {
    let code = match key {
        29..=54 => [
            0x1e, 0x30, 0x2e, 0x20, 0x12, 0x21, 0x22, 0x23, 0x17, 0x24, 0x25, 0x26, 0x32, 0x31,
            0x18, 0x19, 0x10, 0x13, 0x1f, 0x14, 0x16, 0x2f, 0x11, 0x2d, 0x15, 0x2c,
        ][(key - 29) as usize],
        7 => 0x0b,
        8..=16 => (key - 6) as u8,
        55 => 0x33,
        56 => 0x34,
        57 => 0x38,
        58 => return Some((0x38, true)),
        59 => 0x2a,
        60 => 0x36,
        61 => 0x0f,
        62 => 0x39,
        66 => 0x1c,
        67 => 0x0e,
        68 => 0x29,
        69 => 0x0c,
        70 => 0x0d,
        71 => 0x1a,
        72 => 0x1b,
        73 => 0x2b,
        74 => 0x27,
        75 => 0x28,
        76 => 0x35,
        111 => 0x01,
        19 => return Some((0x48, true)),
        20 => return Some((0x50, true)),
        21 => return Some((0x4b, true)),
        22 => return Some((0x4d, true)),
        92 => return Some((0x49, true)),
        93 => return Some((0x51, true)),
        112 => return Some((0x53, true)),
        113 => 0x1d,
        114 => return Some((0x1d, true)),
        115 => 0x3a,
        116 => 0x46,
        117 => return Some((0x5b, true)),
        118 => return Some((0x5c, true)),
        120 => return Some((0x37, true)),
        122 => return Some((0x47, true)),
        123 => return Some((0x4f, true)),
        124 => return Some((0x52, true)),
        131..=140 => (key - 131 + 0x3b) as u8,
        141 => 0x57,
        142 => 0x58,
        143 => 0x45,
        144..=153 => {
            [0x52, 0x4f, 0x50, 0x51, 0x4b, 0x4c, 0x4d, 0x47, 0x48, 0x49][(key - 144) as usize]
        }
        154 => return Some((0x35, true)),
        155 => 0x37,
        156 => 0x4a,
        157 => 0x4e,
        158 => 0x53,
        160 => return Some((0x1c, true)),
        161 => 0x59,
        _ => return None,
    };
    Some((code, false))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn native_mapping_preserves_numpad_locks_and_right_modifiers() {
        assert_eq!(android_scan_code(7), Some((0x0b, false)));
        assert_eq!(android_scan_code(144), Some((0x52, false)));
        assert_eq!(android_scan_code(160), Some((0x1c, true)));
        assert_eq!(android_scan_code(114), Some((0x1d, true)));
        assert_eq!(android_scan_code(115), Some((0x3a, false)));
        assert_eq!(android_scan_code(143), Some((0x45, false)));
        assert_eq!(android_scan_code(142), Some((0x58, false)));
        assert_eq!(android_scan_code(4), None); // Android Back stays local.
    }
    #[test]
    fn capture_preserves_repeat_and_release_after_focus_loss() {
        drain().unwrap();
        set_remote_focus(false);
        assert!(!capture(29, 2, 0));
        set_remote_focus(true);
        assert!(capture(29, 2, 0));
        assert!(capture(29, 2, 0));
        set_remote_focus(false);
        set_remote_focus(true);
        assert!(capture(29, 2, 0)); // consumed, never replayed to the new view
        assert!(capture(29, 2, 1));
        set_remote_focus(false);
        let events = drain().unwrap();
        assert_eq!(events.len(), 3);
        assert!(events[0].pressed && events[1].pressed && !events[2].pressed);
    }
}
