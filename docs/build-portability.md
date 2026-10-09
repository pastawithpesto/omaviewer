# Building and installing OmaViewer on another machine

The binaries keep their existing names: `windowlink-host`, `hypr-rdp`, and
`app-debug.apk`. Build scripts resolve tools from the environment or system,
then use the old checkout's `tools/` installations when available. Those large
SDKs and toolchains are ignored by Git and are not required in a fresh checkout.

## Android APK

Install JDK **17**, Gradle **8.11.1**, Android SDK Platform **35**, Android NDK
**27.2.12479018**, Rust **1.95 or newer**, and `cargo-ndk`. Android Gradle Plugin
remains pinned to **8.9.2** in `android/build.gradle.kts`. A C/C++ build toolchain
and `make` may also be needed by native dependencies. Use SDK Manager to install
the SDK/NDK packages; set `ANDROID_HOME` to the SDK's absolute directory.

```sh
export JAVA_HOME=/path/to/jdk17
export ANDROID_HOME=/path/to/android-sdk
rustup target add aarch64-linux-android
cargo install cargo-ndk --locked
./tools/build-android.sh
```

The output is `android/app/build/outputs/apk/debug/app-debug.apk`. The default
ABI is `arm64-v8a`. To also support an x86_64 emulator:

```sh
rustup target add aarch64-linux-android x86_64-linux-android
ANDROID_ABIS=arm64-v8a,x86_64 ./tools/build-android.sh
```

Use `ANDROID_ABIS=x86_64` for an emulator-only APK. The selected Rust toolchain
must have every requested target installed. The script reports missing targets
before running Gradle; it never installs tools automatically.

| Setting | Selection |
| --- | --- |
| `JAVA_HOME` | Explicit JDK 17; otherwise a system Java 17, then `tools/java/jdk17` |
| `ANDROID_HOME`, `ANDROID_SDK_ROOT` | Explicit SDK; otherwise common Linux/macOS SDK locations, then `tools/android-sdk`. If both variables are set, they must agree. |
| `ANDROID_NDK_HOME`, `ANDROID_NDK_ROOT` | Explicit NDK directory; otherwise `$ANDROID_HOME/ndk/$ANDROID_NDK_VERSION` |
| `ANDROID_NDK_VERSION` | Default `27.2.12479018`; an explicit NDK directory supplies its revision when this variable is unset |
| `GRADLE_BIN`, `GRADLE_HOME` | Explicit executable/installation, then `gradle` on PATH, then `tools/gradle/gradle-8.11.1` |
| `CARGO_HOME`, `RUSTUP_HOME`, `RUSTUP_TOOLCHAIN` | Standard Rust overrides. Otherwise use system Rust if its Android targets exist, then the existing `tools/rustup` fallback. |
| `ANDROID_ABIS` | Comma-separated `arm64-v8a` and/or `x86_64`; default `arm64-v8a` |
| `ANDROID_USER_HOME` | Defaults to `$HOME/.android` to preserve the existing debug signing identity regardless of `XDG_CONFIG_HOME` |
| `GRADLE_OFFLINE` | `1` enables Gradle offline mode; default allows downloads |

To use dependencies already cached in the current checkout:

```sh
./tools/build-android.sh --offline
```

Extra arguments are forwarded to Gradle. `--dry-run` validates the Gradle task
graph without compiling or packaging. Gradle offline mode does not change Cargo's
network mode; set `CARGO_NET_OFFLINE=true` too if all Rust dependencies are cached
and a fully offline build is required.

For direct Gradle use, configure Java, Android, and Cargo on PATH first, then run:

```sh
cd android
gradle :app:assembleDebug -PandroidAbis=arm64-v8a,x86_64
```

Direct Gradle builds also accept `-PandroidNdkVersion=27.2.12479018`; the matching
SDK NDK installation must exist unless an explicit NDK directory is selected.
Prefer the environment settings when using `tools/build-android.sh`, so its
preflight checks and Gradle select the same ABI list and NDK.

Gradle rebuilds Rust when native sources, assets, shared/vendor dependencies,
Cargo configuration, selected ABIs, or relevant toolchain settings change. Native
libraries are generated under `android/app/build/generated/rustJniLibs`; every
rebuild clears that directory and copies `libc++_shared.so` from the selected NDK
for each ABI. Legacy files in `android/app/src/main/jniLibs` are excluded from
packaging. A clean build no longer depends on a manually copied C++ runtime.

The application ID and debug key location remain unchanged. A fresh machine
creates its own debug key unless you explicitly provision your existing key;
Android requires the same signing key to update an already installed APK.

## Linux host

Install Rust and a C build toolchain. To install into your own account:

```sh
./tools/install-host.sh --prefix "$HOME/.local"
export PATH="$HOME/.local/bin:$PATH"
```

The installer uses `host/target/release/windowlink-host` when present, or builds
it with `cargo build --release --locked`. `CARGO_TARGET_DIR` selects another build
output directory. Use `--build` to rebuild after source changes, or
`--binary /path/to/windowlink-host` to install a prebuilt executable. A missing
explicit binary is an error. Review paths without changing anything:

```sh
./tools/install-host.sh --dry-run --prefix "$HOME/.local"
```

The installer also copies `tools/hypr-rdp/target/release/hypr-rdp` if it is present.
Select another existing backend with `--hypr-rdp /path/to/hypr-rdp`, or skip the
backend with `--no-backend`. It never builds the backend automatically and reports
when no backend was installed. Installing the host alone is supported, but RDP
sessions need `hypr-rdp` on PATH or the host's configured backend path. The host
requires a running supported Hyprland session for desktop capture/control.

The backend has additional native dependencies; follow its
[build instructions](../tools/hypr-rdp/README.md#build-from-source) if its source
checkout is available. These include development headers for VA-API, PipeWire,
libxkbcommon, Wayland, GBM, and PAM. Backend binaries must match the destination
machine's CPU architecture and system libraries. The customized backend source is included in this repository as ordinary files; no submodule or separate source checkout is required.

The installer writes only executables under the selected prefix. It does not
start a service, change Hyprland settings, or provision credentials.
