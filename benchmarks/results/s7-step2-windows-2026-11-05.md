# S7 step 2 (one client per profile) — Windows, 2026-11-05

Branch: `codex/windows-environment` (local only). Release client, local dev/APM build.
No input injection, desktop switching, focus acquisition, capture or real audio.

## Trial (`benchmarks/scripts/s7-instance-trial.ps1`)

Four release clients on one isolated invisible desktop, simulated audio. App-window
visibility sampled every 100 ms (V = shown, h = hidden; winit's message window ignored).

| Client | Role | Software | OpenGL (FemtoVG) |
|---|---|---|---|
| A | profile P, guarded, closes to the tray at 1.5 s | hidden 1.9 s, then restored by B; exit 0 | same; exit 0 |
| B | profile P, guarded, started at 3.5 s | `forwarded=true` after 0.1 s, no app window ever; exit 0 | same |
| C | profile P, scripted smoke run without `--single-instance` | starts and shows its window as before; exit 0 | same |
| D | profile Q, guarded | starts normally (other profiles unaffected); exit 0 | same |

Foreground samples owned by any client: 0 (both renderers).

Traces (software): A `hVVVVVVVVVVVVVhhhhhhhhhhhhhhhhhhhVVVV…`, B `h`, C/D `hVVV…`.

Regression after the change: `s7-tray-trial.ps1` (software) and the release startup smoke
pass.

## How it works

A named mutex `Local\turbo-pancake-<profile>` decides which process is the profile's
client. A later launch finds the running client's hidden tray window (title names the
profile), allows it the foreground (`AllowSetForegroundWindow`: the user started this
launch) and posts the same message as a click on the tray icon; it then exits. If the
running client is still starting, the window is looked for for up to 3 s; without an
answer the launch still exits rather than start a second session.

Not covered automatically: a real user double-clicking the shortcut while the first
client is in the tray on the user's desktop (the foreground hand-over is a Windows
policy decision there) — owner-assisted interactive check.
