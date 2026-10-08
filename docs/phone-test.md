# Test results and known problems

## Build results

Tests used a clean Git checkout. The computer supplied the build tools and existing dependency caches.

These builds passed:

- Host release build
- Modified Hypr-RDP release build
- ARM64 Android debug APK

The source repository does not contain SDKs or build outputs. GitHub Actions has not run. The repository is still local.

## Automated tests

| Component | Passed | Skipped |
| --- | --- | --- |
| Android | 69 | 0 |
| Host | 33 | 0 |
| Shared code | 21 | 0 |
| Hypr-RDP | 709 | 4 |
| Total | 832 | 4 |

Four Hypr-RDP tests need specific hardware or mounted file systems. These tests did not run.

## Phone tests

Tests used a Motorola ARM64 phone.

- The APK installed through Wi-Fi ADB.
- The app connected through Tailscale.
- Saved settings remained after an app restart.
- The Settings menu worked in portrait and landscape views.
- The landscape menu scrolled to its lower controls.
- The image clipboard switch kept its setting after a force-stop and restart.

Earlier tests checked window capture, right-click, dragging, keyboard input, and terminal sessions. File tests saved two files and a folder. Each saved file matched the source file, byte for byte.

## Host configuration tests

A separate host installation used a directory name that contained spaces.

Tests confirmed these settings:

- Connection ports and username
- Runtime directory
- Video bitrate
- Upload chunk size
- Command-line FPS value in place of the JSON value

## Tests still required

- Image clipboard transfers in both directions
- Android Share
- File transfer recovery after a connection stops
- Bluetooth keyboards
- Multiple physical monitors
- More phone models

## Known problems

Some separate dialog windows need manual selection. Automatic selection of these windows is not complete.

Rotation during app startup closed the app in one test. A force-stop and restart restored it. Rotation after startup worked.

The local test journal contained personal machine details. This repository contains a summary of the results.
