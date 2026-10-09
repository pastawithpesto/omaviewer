# windowlink-host

`windowlink-host serve` starts the installed `hypr-rdp` server and a small
control API. The RDP endpoint is printed as one JSON object on stdout. It
contains `host`, `port`, `username`, and `password`, plus `control_host`,
`control_port`, and `control_token` for the API. The API token is the same
random secret as the RDP password.

The control listener defaults to the machine's Tailscale IPv4 address on
port `3391`. This preserves the existing Tailscale requirement unless you
explicitly set `--control-bind IP` or `control_bind` in the configuration.
Requests must include `Authorization: Bearer <control_token>`.

## Host configuration and portability

The host requires Linux, an active Hyprland desktop, and the patched `hypr-rdp`
backend shipped in this repository. It does not currently implement capture
for other compositors, macOS, or Windows. Clipboard commands require Wayland
clipboard utilities. File and terminal requests run as the current desktop
user. Connection addresses, account names, storage paths, and backend paths
are configurable; no developer username or home directory is required.

Settings load from `$XDG_CONFIG_HOME/omaviewer/host.json`, falling back to
`~/.config/omaviewer/host.json`. A missing default file uses built-in defaults.
`--config FILE` selects a required JSON file; unknown keys, malformed values,
and out-of-range settings fail before server startup. CLI options override
JSON values. `check` prints JSON capabilities and dependency errors even if Hyprland or the
backend is missing, then exits with status 1 when capture is unavailable.
The complete schema with defaults is in
[`config.example.json`](config.example.json).

```sh
# Existing Tailscale behavior; restrict RDP to Tailscale as well.
windowlink-host serve --tailscale

# Local testing without Tailscale.
windowlink-host serve --bind 127.0.0.1:3389 --control-bind 127.0.0.1

# Explicit trusted LAN interface, custom credentials and video settings.
windowlink-host serve --config ./host.json --bind 192.168.1.20:3389 \
  --control-bind 192.168.1.20 --username alice --fps 60 --bitrate 20000000

# Installed backend at a custom location.
windowlink-host check --backend /opt/omaviewer/bin/hypr-rdp
```

| JSON key | CLI option | Default / meaning |
| --- | --- | --- |
| `bind` | `--bind IP:PORT` | `0.0.0.0:3389`; IPv4 or bracketed IPv6 socket |
| `tailscale` | `--tailscale` | `false`; use Tailscale IPv4 with the configured RDP port |
| `control_bind` | `--control-bind IP` | `null`; discover Tailscale IPv4; explicit IP needs no Tailscale |
| `control_port` | `--control-port PORT` | `3391`; nonzero and different from RDP port |
| `username` | `--username NAME` | `windowlink` |
| `backend` | `--backend PATH` | `hypr-rdp`, resolved through `PATH`; explicit path also accepted |
| `output` / `window` | `--output NAME` / `--window ADDRESS` | `null`; focused monitor; choose one |
| `runtime_dir` | `--runtime-dir DIR` | `$XDG_RUNTIME_DIR`, otherwise `~/.cache/omaviewer/runtime` |
| `password_file` | `--password-file FILE` | `windowlink-hypr-rdp.password` inside runtime directory |
| `endpoint_file` | `--endpoint-file FILE` | `windowlink-main-endpoint.json` inside runtime directory |
| `fps` | `--fps N` | `30`; 1 to 240 frames/second |
| `bitrate` | `--bitrate BPS` | `10000000`; 1 to 1000000000 bits/second |
| `text_clipboard_max_bytes` | `--text-clipboard-max-bytes N` | `1048576` (1 MiB) |
| `image_clipboard_max_bytes` | `--image-clipboard-max-bytes N` | `33554432` (32 MiB encoded) |
| `transfer_chunk_max_bytes` | `--transfer-chunk-max-bytes N` | `4194304` (4 MiB); maximum upload PATCH chunk; 64 KiB to 64 MiB |
| `terminal_history_lines` | `--terminal-history-lines N` | `10000`; 1 to 1000000 |

Clipboard byte limits must be between 1 and 268435456 (256 MiB). The transfer
chunk limit must be between 65536 (64 KiB) and 67108864 (64 MiB) and is
advertised by `GET /fs/roots`. Downloads stream the remaining file with bounded
memory and have no file-size limit. JSON paths must be
absolute UTF-8 paths; `~` and environment variables are not expanded inside
JSON. Runtime storage must be a private directory owned by the current user
(mode 0700); a private directory is created when needed. Password and endpoint
files have mode 0600. Custom file paths require existing parent directories.
A custom password file must contain one nonempty line without control
characters or surrounding whitespace; one trailing LF or CRLF is accepted.
The password is generated once and reused; to rotate it, stop the host and
remove its password file. Changing the username also requires updating clients.
Endpoint files contain credentials and are removed when the host stops.

Explicit LAN or wildcard control binds expose the authenticated control API
on that interface. Use a trusted network or VPN: the control API is plain HTTP
and its bearer token grants desktop, clipboard, terminal, and file access as
the desktop user. Video streaming and capture remain tied to the Hyprland
session, even when connecting through LAN or loopback.

## Routes

- `GET /windows` returns a JSON array of windows across physical outputs.
  Entries include `address`, `title`, `class`, `workspace`,
  `monitor`, `fullscreen`, `size`, and `at`.
- `POST /select` accepts `{"address":"0x..."}` and returns
  `{"selected": <window>}` after the host restarts hypr-rdp with direct
  capture/input targeting for that toplevel. `{"address":null}` switches
  back to desktop capture and responds `{"selected":null}`. The request waits
  for the replacement RDP listener to become ready.
- `GET /preview/{address}` returns an `image/png` snapshot of that individual
  Hyprland toplevel, captured by stable client ID without changing focus. The
  PNG is limited to 8 MiB and the capture has a five-second timeout.
- `GET /clipboard` returns the desktop clipboard as raw UTF-8 `text/plain`
  with a quoted content `ETag`; send that value as `If-None-Match` to get
  `304 Not Modified` and an empty body when unchanged.
- `POST /clipboard` accepts raw UTF-8 `text/plain` and writes it to the
  desktop clipboard. Both directions use `text_clipboard_max_bytes` (1 MiB by default).
- `GET /files` lists regular files in `~/Downloads/WindowLink` as objects
  with `name`, `size` in bytes, and `modified_unix` seconds or `null`.
- `POST /files/{percent-encoded-name}` uploads raw
  `application/octet-stream` bytes (201 created, 200 replaced).
  `GET /files/{percent-encoded-name}` downloads those bytes. File paths are
  restricted to one filename segment. These legacy routes now stream without a file-size limit; the new filesystem API below supports other destinations and resume.

Window selection captures the selected toplevel directly and does not change
its fullscreen state. Separate dialog or popup toplevels must be selected
separately. Keyboard focus is shared with the host desktop; this does not
provide an isolated remote desktop session. The Android app polls this API to
sync plain text while it is in the foreground; RDP clipboard redirection is
unused.


## Filesystem and resumable transfer API

All routes use the same Bearer token. Operations run as the host process's
normal desktop user and reject an elevated root identity. The kernel enforces
permissions; the API does not elevate. Files outside Home are supported.

- `GET /fs/roots` returns `home`, filesystem `root`, and mounted paths.
- `GET /fs/list?path=<percent-encoded-absolute-path>&limit=200&cursor=0`
  returns `path`, `path_id`, `parent`, `parent_id`, `directory_id`, `entries`
  and `next_cursor`. Entries contain `name`, `path`, `path_id`, `kind`
  (`file`, `directory`, `symlink`, `special`), `size`, `modified_unix`,
  `source_id`, and `symlink_target`. Hidden files are included. Pagination
  follows filesystem order; pass `directory_id` with subsequent pages to
  detect directory changes and restart the listing. Maximum page size is 500.
  For non-UTF-8 filenames, `path` is null and `path_id` contains the raw
  absolute path bytes encoded as hexadecimal; use `path_id` in place of
  `path` in query strings and JSON. Display names may contain replacements.
- `POST /fs/mkdir` takes `{"path":"/absolute/new-folder"}` (or `path_id`).
  Transfer folders as individual entries using listings, mkdir and the file
  transfer routes. Symlinks are listed explicitly and not traversed
  automatically; clients must detect visited directories if following links.
- `DELETE /fs/entry?path=...&source_id=...` removes a regular file, link or
  empty folder. Recursive deletion and special file transfers are unsupported.
- `POST /transfers` takes `{"direction":"upload","path":"/absolute/file",
  "size":123,"source_id":"client-source-identity","conflict":"skip"}`.
  Conflict choices are `replace`, `keep_both`, or `skip` (the default).
  Existing destinations return 409 for skip. Uploads are staged beside the
  destination; atomic publication requires a writable destination directory.
  If an existing file is writable but its directory is not, an explicit
  `replace_in_place` choice stages in private runtime storage and copies into
  the checked destination inode. Its descriptor has `atomic:false`; a failed
  final copy can leave that file partially changed. This choice never grants
  additional permissions or follows a final symlink.
  Existing regular-file permission bits are preserved on replacement, and
  new files follow the host's umask. Final symlink destinations are rejected.
  For downloads, use `{"direction":"download","path":"/absolute/file"}`.
  Reading a symlink explicitly follows it, and the opened target must be a
  regular file. Directory handles anchor writes and opened file handles
  anchor reads. Devices, sockets and FIFOs are rejected.
- A successful start returns HTTP 201 with `id`, `direction`, `path`,
  `path_id`, `size`, `offset`, `source_id`, `state`, and `atomic`.
- `GET /transfers/{id}` returns current status and revalidates download source
  identity. Upload identity is supplied by the client, which must revalidate
  its own document before resuming. Downloads use inode, device, length,
  timestamps and mode as their source identity.
- `PATCH /transfers/{id}?offset=N&source_id=...` accepts raw
  `application/octet-stream` with `Content-Length`. The body streams in 64 KiB
  buffers, and there is no total file-size cap. Resume with the returned
  offset after an interruption, and check that the client source still
  matches. The server also checks partial-file identity before appending.
- `POST /transfers/{id}/complete?source_id=...` publishes a complete upload
  after syncing it and checking destination/partial-file identity. New and
  keep-both destinations use atomic no-clobber publication. Replacements
  fail if the observed destination changed during the upload.
- `GET /transfers/{id}/download?offset=N&source_id=...` streams bytes from
  the offset (200 initially, 206 for a resume) with `Content-Length`, `ETag`,
  `X-Source-Id` and `X-Transfer-Offset`. Interrupted responses close without
  appending JSON to binary data. After downloading, recheck transfer status
  before publishing the client file; source changes require a restart.
- `DELETE /transfers/{id}?retain=true` cancels active work and retains a
  resumable partial; `retain=false` deletes the partial and transfer state.
  Reconnect using the same ID and status. Retained IDs last up to 24 hours
  within this host process and are bounded to 32 entries. Cancel old transfers
  when finished. Restart clears IDs; clean shutdown removes retained staging
  files. A crashed process can leave staging files without resumable IDs.

HTTP requests require Content-Length; chunked requests are unsupported. The
header parser authenticates before reading bulk bodies, and 16 bounded
workers let clipboard/window controls continue during transfers. File errors
include 403 for permissions/read-only storage, 409 for conflicts, 415 for
unsupported special files and 507 for insufficient space. No terminal/video
stream is needed for these routes.

## Typed clipboard API

`GET /clipboard/descriptor` returns `mime` (or null), `revision`, `byte_length`
and a MIME-prefixed SHA-256 `content_id`, with a matching quoted ETag.
`GET /clipboard/content` returns raw bytes using that MIME type (204 when no
supported content exists). Send `If-Match` from the descriptor to detect a
changed clipboard (412), or `If-None-Match` for 304. Revisions advance within
the host process when observed content identity changes.

`POST /clipboard/content` takes `text/plain`, `image/png`, or `image/jpeg`.
Text uses `text_clipboard_max_bytes` (1 MiB by default). Images use
`image_clipboard_max_bytes` (32 MiB by default) as their encoded limit,
8192-pixel dimension limits and a 64 MiB decode-allocation limit. PNG bytes
are preserved after validation; JPEG is decoded and converted to PNG before
publishing. Desktop PNG/JPEG offers synchronize as PNG. Commands use explicit
Wayland MIME types and a five-second timeout. Old `/clipboard` text routes
remain compatible.
