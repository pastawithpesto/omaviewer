# OmaViewer Android client

Rust egui/eframe NativeActivity using IronRDP, with a Java bridge for Android clipboard, keyboard and document access.

Supports window and monitor views, saved desktops, special desktop keys, persistent tmux terminals and system-wide file browsing under the desktop user's permissions.

Settings persist theme, terminal font size and alignment, preview interval, independent clipboard switches and a 64 MiB to 4 GiB staging cache. Clipboard sync runs while connected and foregrounded. Text payloads are bounded to 1 MiB, images to 32 MiB. Seekable file transfers have no fixed total file-size cap.

## Build

Follow the [toolchain guide](../docs/build-portability.md), then run from the repository root:

```sh
./tools/build-android.sh
```

ARM64 is the default. `ANDROID_ABIS=arm64-v8a,x86_64` selects both build targets. Output: `android/app/build/outputs/apk/debug/app-debug.apk`. Android 8 or newer is required.

## Tests

```sh
cargo test --locked --manifest-path android/Cargo.toml --lib
```

The optional desktop smoke client runs with `cargo run --manifest-path android/Cargo.toml`. It does not provide Android platform integrations.
