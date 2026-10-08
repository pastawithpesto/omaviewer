# Monitor switching and application dialogs

Status: implementation added; see README and phone-test.md for verified behavior and remaining limits.
Related work: [input controls](input-controls-plan.md).

## Current behavior and reuse

- `host/src/main.rs` chooses an output at startup. Window selection supplies
  `--output`, `--toplevel` and `--input-window` to the existing `hypr-rdp` child.
- `host/src/control.rs` lists windows across outputs. `/select` already has
  target generations and restart acknowledgements. Selecting `address: null`
  returns to the startup output, rather than a monitor chosen on the phone.
- `android/src/window_picker.rs` already fetches windows and previews without
  blocking the UI. Its model currently discards the host's monitor field.
  `android/src/lib.rs` reconnects and resets the viewport after selection.
- Direct capture uses an ext foreign toplevel stable identifier. A separate
  dialog toplevel is a different capture source. Input currently refocuses the
  selected window, which can fight a newly opened dialog.
- Reuse `SharedOutputLayout`, `PresentationGeometry`, `WindowTarget` and the
  shared `Viewport` helpers for geometry. Keep the existing RDP encoder,
  authentication, Tailscale connection and window preview workers.

## Phone behavior

The Desktop tab has a monitor selector, a windows list with monitor names, and
the current target title. Each enabled output is selectable by connector name
and description. Selecting a window does not overwrite the remembered monitor;
Whole monitor returns to that remembered output.

When a supported dialog opens, show it automatically, fit its image to the
phone, and label it with its owning app. Keep Return to app visible. Closing or
completing the dialog returns to the app with its previous zoom and pan. A
nested dialog returns to its nearest surviving parent.

When ownership is uncertain, show a Related window chip with its title and
preview. Opening that chip saves a return target without asserting that the
window belongs to the app. Keep the windows list and Show this monitor usable
when a window closes or a dialog cannot be captured directly. Monitor fallback
shows the whole output and must be labeled accordingly.

## One target model and control contract

Extend the existing control state with a tagged target: `monitor(output)` or
`window(address, stable_id, output)`. Keep `remembered_monitor`, the explicitly
chosen root app, a bounded dialog return stack, and requested and active target
generations. Retain `/select` as an adapter for existing clients.

Proposed authenticated routes:

| Route | Purpose |
| --- | --- |
| `GET /outputs` | Enabled output name, description, logical origin, pixel size, scale, transform and active workspace |
| `POST /target` | Select a monitor or window; validate current output/window identity and return the requested generation |
| `GET /desktop-state` | Requested/active targets and generations, switching/ready/error state, root app, return stack and related candidates |
| `POST /target/return` | Return to a surviving saved app or dialog without closing it |

The host supervisor owns transitions. Reuse child restart and Android reconnect
for the first implementation. A brief reconnect is expected; seamless capture
replacement is not a delivery dependency. Do not report successful sharing
solely because the replacement TCP listener is open: require successful source
creation and current geometry, then enable phone input after its first decoded
frame. Failure retains a usable picker and a precise error.

Use one narrow local child status channel for capture-ready/error messages and
recent remote input markers if the existing supervisor path cannot carry them.
It can also carry a pause/release request for an automatic target transition.
Keep clipboard, files and terminal traffic out of this channel. The existing
serial HTTP listener must not let a selection wait or slow preview prevent
state requests; bound request workers and keep selection work in the supervisor.

Refresh the output list on output changes and reconnect. If the chosen output
disappears, stop sending input and show remaining outputs. If a window moves
outputs, refresh its geometry and capture association. Revalidate stable IDs
before each target use so address reuse cannot select another window.

## Dialog detection and focus

Use a host watcher for Hyprland open, close and active-window events, followed
by validated client snapshots. Reconcile snapshots after watcher reconnect;
event text alone is not the source of truth. Remote input markers establish
when a phone action happened; the host launcher cannot currently see RDP clicks
or keystrokes.

Ownership is an implementation gate:

- Prefer actual parent/transient metadata mapped to stable capture IDs when an
  available backend supplies it. Verify that backend on the supported host.
- The inspected Hyprland 0.56.2 client JSON has `stableId`, `pid`, `class`,
  `xwayland` and geometry, but no parent or modal field. Its wlr foreign
  toplevel implementation does not send a parent event. Binding a protocol
  that defines parent is insufficient.
- Same process, app class, size, focus and time after remote input identify
  candidates; they do not prove parentage. Use them for the related chip.
  Automatic following requires verified parent metadata or a narrowly tested
  application adapter with an explicit ownership rule. Document each adapter
  and its exclusions; generic title matching is insufficient.
- Include a native app's Open/Save flow and a portal file chooser in the initial
  capability checks. Portal choosers can belong to a different process. If
  neither exposes a reliable relationship, implement the needed ownership
  adapter before declaring automatic support for that flow.

Once ownership is supported, save the active target and viewport, pause new
input, discard buffered taps and tap-then-hold deadlines, release held buttons
and keys, and switch capture and input to the same dialog identity. Debounce
repeated events and give explicit phone selection priority. Do not switch on an
unrelated local app activation. On unmap/close,
validate the saved parent and return automatically; if the root closed, show
the picker with no input target.

Change `WindowTarget::prepare` so a newly focused supported dialog is considered
before refocusing the root. Pause new root input while a candidate is being
classified and allow release events to finish. An uncertain candidate must
remain selectable without repeatedly stealing its focus back to the root.
The desktop seat remains shared with the local user.

An `xdg_popup`, subsurface menu or XWayland override-redirect menu may have no
selectable foreign toplevel handle. Test which menus the compositor includes in
direct capture, including content outside the root bounds. Do not invent a
window target for a missing handle. Offer the owning monitor when necessary;
that fallback permits shared desktop input and does not provide window
isolation. Monitor mode already displays visible dialogs on its selected
output; it should not automatically change outputs on unrelated focus events.

## Geometry and cursor dependency

Use one validated geometry snapshot for capture, input and the visible cursor.
It includes target generation, geometry revision, source dimensions,
presentation dimensions and content rectangle. The host additionally retains
output logical origin, bounds, scale and transform, plus the selected window's
logical origin and size. Android does not translate into global desktop space.

Mapping remains: phone image rectangle and viewport crop to presentation
pixels, presentation to source pixels, then host source to logical desktop
coordinates. Cursor rendering applies the inverse presentation/view transform
and clips to the displayed image. Preserve negative output origins, rotated
outputs, fractional scale and letterboxing without applying scale twice.

On monitor, window, dialog or framebuffer change, release held input using old
geometry, cancel gestures, discard buffered single/double taps and hold
deadlines, clear the optional armed right-click action and stale cursor state,
and wait for current geometry and a matching frame. Pending gesture targets,
timers and queued click pairs carry both target generation and geometry
revision; invalidate them before adopting either replacement. Suppress gesture
recognition until existing contacts end. With restart-based
switching, associate each client connection/event queue with its active target
generation; no new per-frame RDP wire field is required. Reject old connection
events even when old and new targets have identical dimensions. No buffered tap,
expired hold or queued click may reach a replacement target. Releases belong to
the old target/session; reconnect never replays them or pending presses. A
geometry change during drag explicitly cancels/releases it before adopting new
bounds.

Save the root viewport independently from the fitted dialog viewport. Restore
it only if its stable identity still exists; clamp it after a resize. Cursor
position and shape handling, trackpad movement, and zoom following are detailed
in [input controls](input-controls-plan.md).

## Delivery and phone acceptance

1. Add output/target state and a remembered monitor using existing restart and
   reconnect paths. Verify state during successful and failed switches.
2. Establish tested dialog ownership, add event watching and automatic
   dialog/return transitions, and update the root focus guard.
3. Connect shared geometry and target generations to input and cursor state.
4. Complete these checks on the actual phone with the existing desktop flow:

| Scenario | Required result |
| --- | --- |
| Two outputs with different sizes, scale, rotation and negative origins | Selector opens the requested output; corner and center clicks land correctly |
| Open a window on another output, then Whole monitor | Returns to the remembered monitor rather than the startup or window output |
| Rapid target switching and reconnect | Latest explicit selection wins; old frames, clicks and cursor events cannot affect it |
| Supported native Open/Save dialog, including another output | Dialog appears automatically, accepts clicks and typing, then returns with saved app zoom |
| Supported portal chooser and nested confirmation | Verified ownership adapter follows each target and returns through surviving parents |
| Another app or another window of the same app opens locally | No automatic takeover based solely on PID, class or title |
| Uncertain dialog, popup menu or missing capture handle | Related chip or clearly labeled monitor fallback remains usable |
| Parent closes, target address is reused, output is unplugged | Input stops; picker remains usable; no other app inherits queued input |
| Dialog opens during a held drag or modifier | Releases/cancellation happen before switching; no stuck input on return |
| Dialog opens or a monitor switch starts during a buffered tap or second-contact hold; repeat during geometry replacement | Pending taps, hold deadlines and queued clicks are discarded; no left/right click reaches the new target; held contacts must end before recognition resumes |
| Zoom, keyboard resize, target resize and local mouse movement | Visible cursor and clicks agree with the image and current geometry |

Record which native, portal and XWayland flows support automatic following.
Unsupported ownership and popup capture are explicit limitations, not passing
results. Update host and phone documentation after these checks pass.

## Source checks

The parent metadata limitation is supported by the inspected local client JSON
and the matching [Hyprland client serialization](https://github.com/hyprwm/Hyprland/blob/v0.56.2/src/debug/HyprCtl.cpp)
and [wlr foreign toplevel implementation](https://github.com/hyprwm/Hyprland/blob/v0.56.2/src/protocols/ForeignToplevelWlr.cpp).
The installed ext foreign toplevel XML exposes identity and listing rather
than ownership; see the [protocol source](https://gitlab.freedesktop.org/wayland/wayland-protocols/-/blob/main/staging/ext-foreign-toplevel-list/ext-foreign-toplevel-list-v1.xml).
Use [Hyprland IPC](https://wiki.hypr.land/IPC/) for event watching and validate
capabilities against the installed version during implementation.
