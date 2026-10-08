# BubblePlay mark exports

The approved master is `bubbleplay-icon.svg`. Regenerate all Android, Windows and web exports and the visual proof sheets with:

```powershell
python branding/build-icons.py
python branding/test_build_icons.py
```

Requires Python 3.10+, Pillow, and an installed Chrome/Chromium (or set `CHROME` to its path). `--no-preview` skips proof sheets. `--followups` regenerates Windows exports and both inline login marks/previews without writing Android resources or web favicons. Chromium uses an isolated temporary profile and no sandbox for these trusted local render inputs.

`preview/android-launcher.png` shows the 66dp safe circle and circle/squircle masks; `android-monochrome.png` shows the themed layer. `windows-ico.png` shows frames at actual size and enlarged on #F3F3F3 and #202020 taskbars. The 16/20/24px transparent frames use hand-hinted native-pixel geometry in `small_frame()` with opaque charcoal, white and apricot pixels plus a one-pixel milk keyline. At those sizes the grey lower facet and blush are omitted to keep the face readable; `small/*.png` are generated copies of the exact ICO frames. Larger frames and web favicons use a rounded milk field. The approved source artwork is unchanged.

`login-desktop.png` and `login-narrow.png` render the Worker login asset. `login-go-desktop.png` and `login-go-narrow.png` render the Go login template with synthetic nonce and approved-browser state. These use each login's CSP in a local preview server; they are visual fixtures, not a deployed authentication test.

Windows executable resources are compiled by the installed Windows SDK `rc.exe` from `host/build.rs` (MSVC; override its path with `RC`). No Rust build dependency is needed. The notification area loads resource 1 from that ICO with `LoadImageW` at the system-DPI small-icon size. Host and input-agent status stay in the existing tooltip and menu text. `out/tray-32.bgra` remains an unused alternative raster.
