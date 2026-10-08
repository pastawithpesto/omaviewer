# Component integration

- `host/` launches the customized Hypr-RDP backend and serves an authenticated HTTP control API. Tailscale is the default control interface; an explicit bind address enables other trusted networks.
- `tools/hypr-rdp/` owns Hyprland capture, RDP graphics and desktop input. Window selection restarts the encoder while the host API stays available.
- `android/` owns the IronRDP session, egui interface, Android clipboard and document-picker bridges. Network and transfer work runs outside the UI thread.
- `shared/` contains viewport geometry, transfer helpers and clipboard propagation state.

The live image path is capture, H.264 over RDP, IronRDP framebuffer, then an egui texture. Overview previews refresh periodically; inactive views pause live graphics. Frame acknowledgments and client backlog guide adaptive FPS and bitrate.

Clipboard startup records both sides without overwriting either. Later changes synchronize while the Android app is foregrounded; simultaneous changes favor the desktop. Text and image synchronization can be disabled independently.

Files use `/fs/roots`, `/fs/list` and `/transfers` with the logged-in desktop user's permissions. Upload chunks obey the advertised limit. Downloads use bounded buffers without a fixed total file-size cap. The older `/files` route remains for compatibility and has different limits; it is not the primary file browser API.

Terminals use tmux and start in the desktop user's home directory. Detaching the phone preserves sessions. A computer reboot ends the processes.

See the [host API reference](../host/README.md) for routes and configuration. All control requests require a bearer token. The API itself is HTTP; Tailscale provides transport encryption in the recommended setup.
