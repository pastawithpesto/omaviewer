# Verification and known issues

## Early preview validation

- The clean Git checkout passed 832 Rust tests: 69 Android, 33 host, 21 shared and 709 backend. Four backend tests that need specific hardware or mounts were ignored. Toolchains and dependency/build caches were supplied externally; no SDK or build output is part of the checkout.
- 69 Android, 33 host and 21 shared Rust tests passed before release preparation.
- The ARM64 APK was installed on a physical Motorola phone over Wi-Fi ADB.
- The app reconnected over Tailscale with saved settings preserved.
- Settings were checked in portrait and landscape. The landscape menu scrolls to its lower controls.
- Disabling image clipboard sync survived a force-stop and relaunch. Both clipboard switches were restored afterward.
- A separately installed host worked from a path containing spaces, with custom loopback ports, username, runtime directory, bitrate and upload chunk size. CLI FPS settings overrode JSON.
- Earlier device tests covered direct capture, tap-then-hold right-click, dragging, terminal keyboard input, persistent sessions, and saving multiple files and a folder with byte-for-byte comparison.

CI adds repeatable build and unit-test coverage. It does not replace an actual Hyprland session and phone.

## Remaining device coverage

- Image clipboard round trips, Android Share and interrupted SAF transfer recovery
- Bluetooth keyboards, multiple physical monitors and additional Android devices
- Automatic following of separate native and portal dialogs
- Rotation immediately during cold launch can end the native activity; force-stop and reopen. Rotation after startup worked. Native-loop failures are logged for diagnosis.

The original local test journal and desktop screenshots contained personal machine details and are excluded from this public source snapshot.
