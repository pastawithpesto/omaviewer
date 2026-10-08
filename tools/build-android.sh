#!/usr/bin/env bash
set -euo pipefail
project_root="$(cd "$(dirname "$0")/.." && pwd)"
fail() { printf 'Android build: %s\n' "$*" >&2; exit 1; }
if [[ ${1:-} == --help || ${1:-} == -h ]]; then
    cat <<'HELP'
Usage: tools/build-android.sh [Gradle options]
Builds app-debug.apk. Extra arguments are forwarded to Gradle (e.g. --offline).
Environment overrides:
  JAVA_HOME                    JDK 17
  ANDROID_HOME / ANDROID_SDK_ROOT  Android SDK (Platform 35)
  ANDROID_NDK_HOME / ANDROID_NDK_ROOT  NDK directory
  ANDROID_NDK_VERSION           SDK NDK version (default 27.2.12479018)
  ANDROID_ABIS                 Comma-separated arm64-v8a,x86_64 (default arm64-v8a)
  GRADLE_BIN / GRADLE_HOME      Gradle executable / installation
  GRADLE_OFFLINE               Set to 1 for cached dependencies only
  CARGO_HOME / RUSTUP_HOME / RUSTUP_TOOLCHAIN  Standard Rust overrides
Configured and system tools take precedence over repository tools/ fallbacks.
HELP
    exit 0
fi

# Preserve the existing debug signing identity across XDG_CONFIG_HOME changes.
export ANDROID_USER_HOME="${ANDROID_USER_HOME:-$HOME/.android}"
export ANDROID_ABIS="${ANDROID_ABIS:-arm64-v8a}"
# A directory override selects its own revision unless a version is explicit.
ndk_override="${ANDROID_NDK_HOME:-${ANDROID_NDK_ROOT:-}}"
if [[ -z ${ANDROID_NDK_VERSION:-} && -f "$ndk_override/source.properties" ]]; then
    ANDROID_NDK_VERSION="$(sed -n 's/^Pkg.Revision[[:space:]]*=[[:space:]]*//p' "$ndk_override/source.properties" | tr -d '\r')"
fi
export ANDROID_NDK_VERSION="${ANDROID_NDK_VERSION:-27.2.12479018}"

if [[ -z ${JAVA_HOME:-} ]]; then
    java_bin="$(command -v java || true)"
    if [[ -n "$java_bin" ]] && "$java_bin" -version 2>&1 | head -1 | grep -q '"17[.\"]'; then
        export JAVA_HOME="$(dirname "$(dirname "$(readlink -f "$java_bin")")")"
    elif [[ -x "$project_root/tools/java/jdk17/bin/java" ]]; then
        export JAVA_HOME="$project_root/tools/java/jdk17"
    else
        fail 'Set JAVA_HOME to a JDK 17 installation.'
    fi
fi
[[ -x "$JAVA_HOME/bin/javac" ]] || fail "JAVA_HOME is not a JDK: $JAVA_HOME"
"$JAVA_HOME/bin/java" -version 2>&1 | head -1 | grep -q '"17[.\"]' || fail 'JAVA_HOME must select JDK 17.'
export PATH="$JAVA_HOME/bin:$PATH"

if [[ -n ${ANDROID_HOME:-} && -n ${ANDROID_SDK_ROOT:-} && "$ANDROID_HOME" != "$ANDROID_SDK_ROOT" ]]; then
    fail 'ANDROID_HOME and ANDROID_SDK_ROOT must refer to the same SDK.'
fi
sdk="${ANDROID_HOME:-${ANDROID_SDK_ROOT:-}}"
if [[ -z "$sdk" ]]; then
    for candidate in "$HOME/Android/Sdk" "$HOME/Library/Android/sdk" /opt/android-sdk "$project_root/tools/android-sdk"; do
        if [[ -d "$candidate" ]]; then sdk="$candidate"; break; fi
    done
fi
[[ -n "$sdk" && -d "$sdk" ]] || fail 'Set ANDROID_HOME to an installed Android SDK.'
export ANDROID_HOME="$sdk" ANDROID_SDK_ROOT="$sdk"
export ANDROID_NDK_HOME="${ANDROID_NDK_HOME:-${ANDROID_NDK_ROOT:-$sdk/ndk/$ANDROID_NDK_VERSION}}"
[[ -f "$ANDROID_NDK_HOME/source.properties" ]] || fail "NDK missing at $ANDROID_NDK_HOME; install NDK $ANDROID_NDK_VERSION or set ANDROID_NDK_HOME."

# Use a working system Rust first. The old checkout's bundled rustup remains a
# fallback when the system compiler has no Android standard library installed.
export CARGO_HOME="${CARGO_HOME:-$HOME/.cargo}"
IFS=',' read -ra abis <<< "$ANDROID_ABIS"
(( ${#abis[@]} )) || fail 'ANDROID_ABIS cannot be empty.'
targets=()
for abi in "${abis[@]}"; do
    case "$abi" in
        arm64-v8a) targets+=(aarch64-linux-android) ;;
        x86_64) targets+=(x86_64-linux-android) ;;
        *) fail "Unsupported ABI '$abi'; choose arm64-v8a and/or x86_64." ;;
    esac
done
has_targets() {
    local target libdir
    command -v rustc >/dev/null 2>&1 || return 1
    for target in "${targets[@]}"; do
        libdir="$(rustc --print target-libdir --target "$target" 2>/dev/null)" || return 1
        compgen -G "$libdir/libstd-*.rlib" >/dev/null || return 1
    done
}
if [[ -n ${RUSTUP_HOME:-} || -n ${RUSTUP_TOOLCHAIN:-} ]]; then
    export PATH="$CARGO_HOME/bin:$PATH"
elif ! has_targets && [[ -f "$project_root/tools/rustup/settings.toml" && -x "$CARGO_HOME/bin/rustup" ]]; then
    export RUSTUP_HOME="$project_root/tools/rustup"
    export PATH="$CARGO_HOME/bin:$PATH"
fi
# cargo-ndk is commonly installed here, even with a system Cargo installation.
export PATH="$PATH:$CARGO_HOME/bin"
command -v cargo >/dev/null || fail 'Install Cargo/Rust and cargo-ndk.'
command -v cargo-ndk >/dev/null || fail 'Install cargo-ndk with: cargo install cargo-ndk --locked'
has_targets || fail "Android Rust targets missing; install with: rustup target add ${targets[*]} (using the selected RUSTUP_HOME/toolchain)."

if [[ -n ${GRADLE_BIN:-} ]]; then
    gradle_bin="$GRADLE_BIN"
elif [[ -n ${GRADLE_HOME:-} ]]; then
    gradle_bin="$GRADLE_HOME/bin/gradle"
elif command -v gradle >/dev/null; then
    gradle_bin="$(command -v gradle)"
else
    gradle_bin="$project_root/tools/gradle/gradle-8.11.1/bin/gradle"
fi
command -v "$gradle_bin" >/dev/null || fail 'Install Gradle 8.11.1 or set GRADLE_BIN/GRADLE_HOME.'
args=(--no-daemon)
[[ ${GRADLE_OFFLINE:-0} != 1 ]] || args+=(--offline)
cd "$project_root/android"
exec "$gradle_bin" "${args[@]}" :app:assembleDebug "$@"
