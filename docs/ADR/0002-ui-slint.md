# ADR 0002 — Desktop UI toolkit: Slint (provisional)

* Status: **provisional** (S0, 2026-11-02). Final confirmation requires the open items below.
* Spec: §3 (Slint as primary candidate, confirmed by prototype), §9, §13, §14.6

## Context

The spec requires a native UI without Electron/browser runtime, Discord-like layout,
dark/light themes, HiDPI, tray, smooth rendering, ≤ 50 MiB idle RAM and ≤ 30 MB installer.
Slint is the preferred candidate; the choice must be confirmed by a prototype that checks
GPU texture access, text quality, animations, themes, RAM and package size.

## Prototype (implemented)

`apps/desktop`: Slint 1.18.1, features `backend-winit`, `renderer-femtovg` (OpenGL),
`renderer-software`, `accessibility`, `unstable-winit-030`. Qt backend excluded. UI in
`apps/desktop/ui/main.slint`: sidebar (DM list, search placeholder, user panel), header,
empty state, dark/light theme toggle (`Theme` global), keyboard-focusable button.

## Measurements (Windows 11, Ryzen 5 5500, RTX 2060 SUPER, driver 617.14, 1920×1080 @100 %)

`benchmarks/results/desktop-startup-*-windows-*.json`, 7 runs each, window on the secondary
monitor, created inactive, memory sampled 3 s after the first frame:

| Renderer | First frame (p50, spawn→paint) | Working set (p50) | Private bytes (p50) |
|---|---|---|---|
| FemtoVG / OpenGL (default) | ≈ 0.40 s | ≈ 71.5 MiB | ≈ 76.7 MiB |
| Software (`SLINT_BACKEND=winit-software`) | ≈ 0.07 s | ≈ 21.8 MiB | ≈ 6.2 MiB |

Release executable: 9.1 MiB uncompressed (`dsmini-desktop.exe`, both renderers + accessibility).

Findings:
* The **OpenGL path alone exceeds the 50 MiB idle target**; the extra ~50 MiB is the NVIDIA
  OpenGL driver/context (private bytes jump), not application data.
* The software renderer fits comfortably (≈ 22 MiB WS) and starts faster, but its CPU cost
  for large/HiDPI windows and animations at monitor refresh rate is **not yet measured**.
* Startup < 2 s holds for both (warm). True cold start (empty file cache) not measured.

## Decision (provisional)

Keep Slint. Default renderer decision is deferred to S7 with data; candidates:
software renderer for chat UI (RAM target) vs. GPU renderer (smoothness), or
`renderer-skia`/`femtovg-wgpu` with D3D/Metal/Vulkan which may have a lower driver
footprint than OpenGL. Video frames (S3+) will need a GPU path regardless.

## Open items (blocking final acceptance of this ADR)

1. **License** — Slint is `GPL-3.0-only OR Slint Royalty-free 2.0 OR Slint Software 3.0`.
   Royalty-free desktop use requires an attribution ("Made with Slint" badge/notice). The
   product owner must confirm the license choice. **UNVERIFIED / owner decision.**
2. **GPU texture import** for video (S3): Slint offers `unstable-wgpu-29/30` (import
   `wgpu::Texture` into `Image`) and borrowed OpenGL textures for FemtoVG. Needs a spike
   with a real decoded frame (D3D11/Metal/VAAPI interop). **UNVERIFIED.**
3. Frame-time p50/p95/p99 during animations/scrolling at 60/144 Hz per renderer. **UNVERIFIED.**
4. Text rendering quality with Cyrillic + emoji at 100/125/150/200 % scaling. Visual check
   at 100 % done (screenshot during S0 smoke); other scales **UNVERIFIED**.
5. macOS (M4) and Linux X11/Wayland measurements. **UNVERIFIED.**

## Alternatives (if Slint fails an item)

`iced` (wgpu-based, MIT) or `egui` (immediate mode, MIT) — require a comparison benchmark
on the same metrics and a new ADR.
