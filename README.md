# OmaLink

Your Omarchy desktop, on your Android phone.

Open one window or a whole monitor, use persistent terminals, and move files between your phone and desktop. Built primarily in Rust with a small Android platform bridge.

**Early preview.** Built for Omarchy with Hyprland. Android 8 or newer is required; ARM64 is the tested phone architecture.

## Features

- Window previews, direct window capture, monitor selection and adaptive video
- Tap to click, tap then hold to right-click, drag, scroll, pinch to zoom and pan
- Super, Ctrl, Alt, Shift, arrows, Tab, Esc and F1–F12
- Multiple persistent tmux terminals with direct phone keyboard input
- File browsing with desktop-user permissions, multiple selection and folder transfers
- Foreground text and image clipboard sync
- Saved desktops, themes, preview intervals and configurable terminal controls

## Start on Omarchy

Use Rust 1.95 or newer. Install build and runtime dependencies:

```sh
sudo pacman -S --needed base-devel clang cmake nasm pkgconf libva pipewire \
  libxkbcommon wayland mesa pam tmux wl-clipboard
```

From this checkout:

```sh
cargo build --release --locked --manifest-path tools/hypr-rdp/Cargo.toml
./tools/install-host.sh --build --prefix "$HOME/.local"
export PATH="$HOME/.local/bin:$PATH"
windowlink-host check
windowlink-host serve --tailscale
```

The customized backend is included in `tools/hypr-rdp/`; no submodule initialization is needed. Run the host inside your Hyprland session as your desktop user.

Connect both devices to your Tailscale network. The host prints a private JSON endpoint containing the address, username, password and control port. Enter these in OmaLink on the phone. Keep this output private.

Build the Android APK using the [build guide](docs/build-portability.md), then install `android/app/build/outputs/apk/debug/app-debug.apk`. Preview APKs use debug signing; they are development builds.

## Settings

The phone's **Settings** menu saves theme, terminal font size and alignment, preview timing, clipboard switches and transfer cache size.

Host settings load from `~/.config/omalink/host.json`, or the XDG configuration directory. Copy [the example](host/config.example.json) and change the fields you need. Command-line options override the file. See the [host reference](host/README.md) for LAN binding, paths, video limits and transfer settings.

The control API uses HTTP with bearer authentication. Use Tailscale for encrypted transport. Explicit LAN binding is intended for trusted networks; do not expose the control port directly to the internet.

## Development

```sh
cargo test --locked --manifest-path shared/Cargo.toml
cargo test --locked --manifest-path host/Cargo.toml
cargo test --locked --manifest-path android/Cargo.toml --lib
```

See [architecture](docs/integration.md), [verification and known issues](docs/phone-test.md), and [build instructions](docs/build-portability.md). The CI workflow builds the host, customized backend and Android APK, and runs Rust tests.

## Known limits

- Separate dialogs sometimes require selecting their window or switching to monitor view.
- Image clipboard, Android Share and interrupted phone file transfers need more device testing.
- Rotating immediately during a cold launch can close the native activity; force-stop and reopen if this happens.
- Android clipboard payloads are bounded to 1 MiB for text and 32 MiB for images.
- Host binaries depend on the destination system libraries. Build on the target Omarchy computer when necessary.

## License and credits

OmaLink code is MIT licensed. Included dependencies keep their own licenses. See [third-party notices](THIRD_PARTY_NOTICES.md).
