#!/usr/bin/env bash
set -euo pipefail
project_root="$(cd "$(dirname "$0")/.." && pwd)"
prefix="${HOME}/.local"
host_binary=""
backend_binary=""
backend_mode=auto
force_build=0
dry_run=0
fail() { printf 'Host installer: %s\n' "$*" >&2; exit 1; }
usage() {
    cat <<'HELP'
Usage: tools/install-host.sh [options]
  --prefix DIR       Install into DIR/bin (default $HOME/.local)
  --binary FILE      Install an existing windowlink-host executable
  --build            Rebuild host with Cargo, even if a release binary exists
  --hypr-rdp FILE    Also install this existing hypr-rdp executable
  --no-backend       Skip optional bundled hypr-rdp binary
  --dry-run          Show actions without building or installing
  -h, --help        Show help
Uses host/target/release/windowlink-host if present; otherwise builds it.
The optional backend is copied from tools/hypr-rdp/target/release/hypr-rdp
if present. Installing the host alone is supported; serve needs hypr-rdp.
HELP
}
while (( $# )); do
    case "$1" in
        --prefix|--binary|--hypr-rdp)
            (( $# >= 2 )) && [[ -n "$2" ]] || fail "$1 requires a value"
            case "$1" in
                --prefix) prefix="$2" ;;
                --binary) host_binary="$2" ;;
                --hypr-rdp) backend_binary="$2"; backend_mode=explicit ;;
            esac
            shift 2 ;;
        --build) force_build=1; shift ;;
        --no-backend) backend_mode=skip; shift ;;
        --dry-run) dry_run=1; shift ;;
        -h|--help) usage; exit 0 ;;
        *) fail "Unknown option: $1" ;;
    esac
done
[[ -z "$host_binary" || "$force_build" == 0 ]] || fail '--binary and --build cannot be combined.'
if [[ -n "$host_binary" ]]; then
    [[ -f "$host_binary" && -x "$host_binary" ]] || fail "Host executable missing or not executable: $host_binary"
else
    # Set the build output explicitly so global Cargo configuration cannot make
    # the installer report success while looking in an unrelated target folder.
    target_dir="${CARGO_TARGET_DIR:-$project_root/host/target}"
    [[ "$target_dir" == /* ]] || target_dir="$PWD/$target_dir"
    host_binary="$target_dir/release/windowlink-host"
    if [[ "$force_build" == 1 || ! -x "$host_binary" ]]; then
        if [[ "$dry_run" == 1 ]]; then
            printf 'Would build host with Cargo into %s\n' "$target_dir"
        else
            command -v cargo >/dev/null || fail 'Cargo is required to build the host; install Rust or use --binary.'
            cargo build --manifest-path "$project_root/host/Cargo.toml" --target-dir "$target_dir" --release --locked
            [[ -f "$host_binary" && -x "$host_binary" ]] || fail "Build did not produce $host_binary"
        fi
    fi
fi
if [[ "$backend_mode" == auto && -x "$project_root/tools/hypr-rdp/target/release/hypr-rdp" ]]; then
    backend_binary="$project_root/tools/hypr-rdp/target/release/hypr-rdp"
elif [[ "$backend_mode" == explicit ]]; then
    [[ -f "$backend_binary" && -x "$backend_binary" ]] || fail "Backend executable missing or not executable: $backend_binary"
elif [[ "$backend_mode" == skip ]]; then
    backend_binary=""
fi
if [[ "$dry_run" == 1 ]]; then
    printf 'Would install %s -> %s/bin/windowlink-host\n' "$host_binary" "$prefix"
    [[ -z "$backend_binary" ]] || printf 'Would install %s -> %s/bin/hypr-rdp\n' "$backend_binary" "$prefix"
    exit 0
fi
install -d "$prefix/bin"
install -m 755 "$host_binary" "$prefix/bin/windowlink-host"
printf 'Installed %s/bin/windowlink-host\n' "$prefix"
if [[ -n "$backend_binary" ]]; then
    install -m 755 "$backend_binary" "$prefix/bin/hypr-rdp"
    printf 'Installed %s/bin/hypr-rdp\n' "$prefix"
else
    printf 'No backend installed; make hypr-rdp available on PATH before starting an RDP session.\n'
fi
printf 'Ensure %s/bin is on PATH.\n' "$prefix"
