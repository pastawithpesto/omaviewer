# OmaLink 0.1.0 preview

Use your Omarchy desktop from your Android phone.

Open an app window or a full monitor, zoom and move around, and control it with touch and desktop keys. Open multiple persistent terminals, paste commands, and transfer files using your desktop account's existing permissions.

## Included

- Window previews and adaptive streaming
- Tap, right-click, drag, scroll, zoom and pan
- Desktop modifiers, arrows, Tab and function keys
- Persistent tmux sessions and direct phone keyboard input
- File and folder transfers with multiple selection
- Text and image clipboard synchronization
- Saved desktops, themes and configurable controls

## Getting started

Build the host on your Omarchy computer and install the Android APK built from this source. Connect both devices through Tailscale and enter the endpoint details printed by the host. The [README](../README.md) has the commands.

This is an early preview. Separate dialogs sometimes need manual selection, several clipboard and transfer flows need broader phone testing, and rotation during cold startup can require reopening the app. See [known issues](phone-test.md).

The source includes the customized Hypr-RDP backend and IronRDP code used by OmaLink. Project code is MIT licensed; dependencies retain their own licenses.
