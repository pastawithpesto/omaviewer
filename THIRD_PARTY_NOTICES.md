# Third-party notices

OmaLink reuses the following projects. Their copyright and license files are retained.

| Component | Source | License and local notice |
| --- | --- | --- |
| Hypr-RDP | [MuNeNICK/hypr-rdp](https://github.com/MuNeNICK/hypr-rdp) | MIT; [LICENSE](tools/hypr-rdp/LICENSE), including OpenH264 notice |
| IronRDP | [Devolutions/IronRDP](https://github.com/Devolutions/IronRDP) | MIT OR Apache-2.0; [MIT](vendor/ironrdp/LICENSE-MIT), [Apache](vendor/ironrdp/LICENSE-APACHE) |
| Alacritty terminal parser | [alacritty/alacritty](https://github.com/alacritty/alacritty) | Apache-2.0; pinned by `android/Cargo.lock` |
| egui and eframe | [emilk/egui](https://github.com/emilk/egui) | MIT OR Apache-2.0; pinned by `android/Cargo.lock` |
| JetBrains Mono Nerd Font | [Nerd Fonts](https://github.com/ryanoasis/nerd-fonts) | SIL Open Font License; [OFL](android/assets/fonts/OFL.txt) |

The Hypr-RDP snapshot starts from commit `7817f2d73db1c141cf02fc68ca5af9c35636e237` plus OmaLink's local capture, input, adaptive-streaming and compatibility changes. The changes are included as ordinary source files. Its upstream README describes upstream behavior; OmaLink's root README is the setup entry point.

The vendored IronRDP snapshot includes local framebuffer/codec fixes. The original import did not record a verifiable upstream revision. This release preserves the imported sources and license files instead of claiming an upstream commit. Cargo lockfiles pin fetched dependencies.

Binary releases must also include dependency license texts. The Android NDK C++ runtime notices must accompany APK distribution. This source repository does not include downloaded installers or private signing keys.
