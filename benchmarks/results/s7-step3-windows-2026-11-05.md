# S7 step 3 (responsive layout and scaling) — Windows, 2026-11-05

Branch: `codex/windows-environment` (local only). Release client, local dev/APM build.
No input injection, desktop switching, focus acquisition, capture or real audio.

## Trial (`benchmarks/scripts/s7-layout-trial.ps1`)

Ten release-client runs, one after another, each on its own isolated invisible desktop
with simulated audio. Each takes a snapshot of its own window (renderer pixels only).
Every case passed with both renderers: snapshot size equals the requested physical size,
expected scale factor in effect, snapshot written, clean exit (code 0).

| Case | Physical size | Scale | View | Result (software / OpenGL) |
|---|---|---|---|---|
| wide-auth | 1280×720 | 1 | sign-in | PASS / PASS |
| narrow-auth | 700×480 | 1 | sign-in, sidebar collapsed to the rail | PASS / PASS |
| narrow-voice | 700×480 | 1 | call, "Звонок" tab | PASS / PASS |
| narrow-screen | 700×480 | 1 | call, "Экран" tab | PASS / PASS |
| narrow-sidebar | 700×480 | 1 | sidebar opened from the rail | PASS / PASS |
| wide-voice | 1280×720 | 1 | call + screen side by side | PASS / PASS |
| hidpi150-voice | 1920×1080 | 1.5 | call (1280×720 logical) | PASS / PASS |
| hidpi200-auth | 1400×900 | 2 | sign-in (700×450 logical, compact) | PASS / PASS |
| hidpi200-settings | 1400×900 | 2 | settings panel | PASS / PASS |
| setting125-settings | 1100×700 | from the setting | `ui_scale=125` × system scale | PASS / PASS: factor 1.5625 = 1.25 × system 1.25 |

## Defects found by visual review and fixed in this step

* Below 860 px logical width the call view clipped the screen panel and the DSP
  checkboxes: the call view now shows one panel at a time ("Звонок" / "Экран" tabs).
* At the call panel's maximum width (560 px) the mute/NS/AEC row was cut at
  "Эхоподавление", also on wide windows at 100 %: the row now wraps (`FlexboxLayout`).
* The ☰ glyph is missing from the fonts used for rendering (blank button): replaced by ≡ / ×.
* A `compact` binding on the window width formed a layout binding loop (Slint
  warning, "may panic at runtime"): it is now set from `changed width`.
* The build line still said "S5": the stage label was dropped.
* The first full test run failed `app_settings::tests::round_trip_and_defaults`: the
  settings file was written without `ui_scale`, so a scale chosen in the panel would have
  been lost at the next start. Fixed; the trial had not seen it because it writes the
  settings file itself.

A requested 900×600 window at factor 1.5625 became 1000×656: the window's minimum size
(640×420 logical) is enforced, as intended.

## Not covered

* Keyboard tab/focus order: not exercised automatically (no input injection on the
  user's machine, and Slint offers no public focus query). Owner-assisted check.
* Moving the window between monitors with different scales while a user scale is set:
  the factor is fixed at start (documented limitation of `SLINT_SCALE_FACTOR`).
* Snapshots are not committed: they show the local account name in the sidebar.
