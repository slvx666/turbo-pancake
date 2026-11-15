# ADR 0019 — The share window shows pictures from the GPU

Status: accepted (2026-11-15). Spec §5.1 (up to 1440p120, minimal copying between GPU
and RAM, the watched share in its own ordinary window and full screen), §13 (UI must not
stall during a call). Supersedes the "GPU texture import" plan of known issues 29, 32, 48
for the share window.

## Context

A decoded picture took this way to the screen: DXVA surface → copy to a staging texture
and map (≈ 3 ms at 1080p) → NV12 → RGBA on the CPU (≈ 4 ms on four threads) → handed to
the UI thread through a 16 ms timer → copied into a Slint image → scaled by the software
renderer. At most ≈ 60 pictures a second reached the screen, at 130–160 % of a core for a
1440p share, on the thread that also runs the interface.

The default renderer stays the software one (ADR 0002: idle RAM), so importing the
texture into the UI toolkit is not available.

## Decision

In the share window the picture is not a UI element. The window gets a native child
window under its bar (`apps/desktop/src/video_surface.rs`); the viewer thread owns a swap
chain on it (`media_video::win::Presenter`) and draws every decoded surface with the
D3D11 video processor — colour conversion, scaling and letterboxing on the GPU — then
presents without waiting for the display. The decoder offers each surface to a sink first
(`H264Decoder::set_surface_sink`); a picture taken there is never copied to the CPU.

* The child window ignores the mouse (`HTTRANSPARENT`): a double click still reaches the
  Slint window (full screen), the bar with the status text and buttons stays a Slint UI.
* The UI names the window through `screen::PresentTarget` and shows the child only while
  pictures really arrive on the GPU path. Software decoding, a failed swap chain or a
  window without area fall back to the old path picture by picture; nothing else changes.
* The embedded view in the main window keeps the CPU path: combo box lists and the
  settings panel are drawn by Slint inside that window and a native child would cover
  them. Watching at full rate is what the share window (and full screen) is for.

## Evidence

* `crates/media-video/tests/present.rs` (hardware GPU, a window that is never shown):
  encoder → DXVA decoder → presenter; the drawn back buffer is read back and compared
  with the source picture (colours within the codec's error, scaling, black bars).
* `s7-renderer-video-trial.ps1 -ShareWindow` (release clients on the invisible desktop,
  generated share, high layer): the viewer reports `gpu_present=true`; for a 2560×1440
  share its process used 14 % of a core instead of 158 %, decode + show 0.57 ms per
  picture instead of 6.7 ms (1080p: 19 %). Shown rate = decoded rate.

## Not verified

* 120 pictures a second end to end: in the trial run the sharer's encoder delivered 60
  (the dev GPU was busy with the owner's game), and the viewer showed all of them.
* The picture on a real screen (tearing, colours by eye, full screen, moving the window
  between monitors with different scales): owner-assisted.
