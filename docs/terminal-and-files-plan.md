# Terminal, desktop controls and file transfer plan

Status: implementation added; see README and phone-test.md for verified behavior and remaining limits.

## Scope

Add persistent terminals, image clipboard sync, integrated sharing, explicit monitor switching, related dialog handling, full keyboard input, precise pointer controls and a visible remote cursor. Expand file transfers to the computer's filesystem wherever the logged-in desktop user has permission. Keep the existing desktop and per-window views. All of these are required implementation scope.

Detailed work packages:

- [Keyboard, right-click, cursor and dragging](input-controls-plan.md)
- [Monitor switching and application dialogs](desktop-targets-plan.md)
- [Image clipboard, sharing and filesystem access](clipboard-files-plan.md)

## Phone interface

- Top navigation: Desktop, Terminal, Files
- Terminal header: Back, named session switcher, New and overflow menu. Paste stays on the key strip and Attach in the menu
- One terminal fills portrait mode. Optional two-pane landscape view comes later
- Tap the terminal to type directly. No typing dialog, separate visible input box or Send button
- Keep Ctrl, Alt, Esc, Tab and arrows above the keyboard, with expandable function keys
- Long-press selects text. Interaction mode passes supported taps and scrolling to the terminal application
- Pinch changes font size and terminal dimensions. Opening the keyboard resizes the terminal and keeps the cursor visible
- Reuse app themes, readable typography and inline transfer progress
- Desktop right-click uses tap, then touch and hold at the same spot. In Trackpad mode it acts at the visible cursor. Show a short gesture hint and hold feedback; a toolbar action is only an accessibility fallback
- Delay committing touch taps until single tap, double tap or tap-then-hold can be distinguished. A successful right-click gesture must never send an earlier left-click. Calibrate the short tap delay on the phone
- Terminal single long-press remains text selection. Define terminal mouse interaction separately so selection never also sends a remote right-click

## Persistent terminals

- Use actual tmux sessions, with an independent shell and working directory for each app terminal
- Support create, rename, switch, detach and explicitly close
- Sessions survive phone disconnection and remain attachable from the desktop
- Keep stable session identifiers across host/app reconnects. A host restart can reattach to surviving tmux sessions; a computer reboot ends running processes
- Stream only visible terminals. Background processes keep running in tmux
- Rust host attaches through a PTY using portable-pty and adds an authenticated bidirectional stream over the existing Tailscale connection
- Run the stream independently of RDP and capture-child restarts, with bounded buffers and backpressure. Detach clients without killing tmux sessions
- On reattach, negotiate terminal size and request a complete redraw. Use existing tmux commands for session names and working directories, without building a second session manager
- Reuse a Rust terminal engine. Evaluate egui_term and its Alacritty backend against our egui version, Android build and remote byte input before choosing the integration
- Reuse keyboard controls with terminal escape sequences and control bytes. Keep the desktop modifier label Super

## Interactive terminal applications

- Test OpenCode, Codex, a shell and a full-screen editor on the actual phone
- Require colors, Unicode, alternate screen, cursor positioning, terminal queries, resizing, mouse reporting and bracketed paste
- Preserve native Android text composition and send committed input once
- Verify multiline prompts, menus, shortcuts, selection, scrolling and reconnect redraws in portrait and landscape
- Keep the application's own interface. App-provided prompts remain inside the terminal
- Uploading a file and inserting its path does not guarantee that every terminal application treats it as an attachment

## Files across the computer

- Run file operations as the logged-in desktop user, preserving their groups and normal operating-system permissions
- Browse from Home, filesystem root, mounted drives or a selected path. Remove the fixed Downloads/WindowLink restriction
- Permit reads and writes wherever that user is allowed. Report permission errors inline and do not elevate privileges
- Upload and download files, create folders, and transfer folder contents recursively
- Let users choose the destination. Terminal Attach uses the selected working folder when known, otherwise the last chosen destination
- After upload, offer Insert path. Quote paths for the active shell; leave execution to the user
- Reuse the Android document picker and add Share to OmaViewer. Text paste remains direct; clipboard file support depends on Android and the source app
- Stream data with bounded memory. Replace the current 100 MiB whole-file limit and support progress, cancellation and resumable transfers
- Preserve explicit conflict choices: replace, keep both or skip. Stage uploads beside the destination and finalize only after completion
- Handle symlinks deliberately, prevent recursive loops, and transfer regular files without reading devices or special files

## Delivery order

1. Establish shared target identity and coordinate mapping for pointer input, cursor rendering, monitor switching and related dialogs
2. Fix discoverable right-click, reliable drag/release, trackpad control, visible cursor and physical keyboard forwarding
3. Add monitor selection and automatic related-dialog handling with a clear return to the parent app
4. Validate the Rust terminal renderer with OpenCode and Codex, then deliver direct typing, persistent sessions, switching, paste and reconnect
5. Add typed image clipboard sync and shared file upload/download actions
6. Expand filesystem browsing and streaming transfers under the desktop user's identity, including folders, cancellation, resume and terminal attachments
7. Run the acceptance checks in each work package on the phone in portrait and landscape, including zoom, keyboard resize, target changes and interrupted connections

Independent work packages may proceed in parallel after agreeing on shared message types. No requested feature moves to optional follow-up work because another package finishes first.

Terminal and file access must work without an active video stream.

## RustDesk comparison and remaining desktop work

Already implemented: direct window capture, whole-monitor view, window previews, adaptive bitrate/FPS, touch input, zoom/pan, on-screen special keys, foreground text clipboard sync and basic file transfers. These features still need regression checks as terminals and file access expand.

Required desktop work alongside the terminal and file milestones:

| Area | Current gap | Planned work |
| --- | --- | --- |
| Physical keyboard | Full forwarding is incomplete | Forward key press/release, modifiers, layouts and shortcuts from Bluetooth/USB keyboards |
| Clipboard | Plain text only | Add image clipboard support where Android and the host support it; integrate file sharing without promising universal Android file clipboard support |
| Multiple monitors | Whole monitor returns to the startup output | Add an explicit monitor selector and remember the selected output |
| Application dialogs | Separate toplevels require manual selection | Automatically follow supported related dialogs, provide a choice when ownership is uncertain, and return to the parent app |
| Files | Fixed directory, 100 MiB limit and basic transfers | Deliver filesystem access, folders, cancellation and resume as specified above |
| Terminal | No terminal mode yet | Deliver persistent tmux sessions and interactive terminal compatibility as specified above |

Implement and verify relative trackpad mode, visible remote cursor, cursor following while zoomed, reliable drag operations and keyboard visibility. Replace the button-first right-click design with tap then tap-and-hold as the primary desktop gesture. Keep the existing menu action only as a fallback. The input work package defines timing, cancellation and separation from dragging.

Later options: remote audio, session recording, view-only mode and optional lock-on-disconnect. Audio needs an end-to-end host and Android check; dependencies containing audio code do not establish working playback.

Use the existing Tailscale transport. RustDesk's discovery/relay infrastructure, cross-platform host support, remote printing and device administration are outside this implementation plan.

RustDesk's [settings reference](https://rustdesk.com/docs/en/self-host/client-configuration/advanced-settings/) and [Android guide](https://rustdesk.com/docs/en/client/android/) are comparison references. Feature availability varies by platform and version. Reuse existing Rust/RDP components first; check compatibility and licensing before importing RustDesk source.

## Reuse candidates

- [tmux](https://github.com/tmux/tmux)
- [portable-pty](https://docs.rs/portable-pty/latest/portable_pty/)
- [egui_term](https://github.com/kemokempo/egui_term)
- Existing authentication, themes, keyboard UI, clipboard bridge, file worker and Android document picker
