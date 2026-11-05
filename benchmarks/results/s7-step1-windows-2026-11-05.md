# S7 step 1 (tray and window lifecycle) — Windows, 2026-11-05

Branch: `codex/windows-environment` (local only). Release client built with the local
dev/APM wrapper. No input injection, desktop switching, focus acquisition, screen
capture or real audio devices.

## Lifecycle trial (`benchmarks/scripts/s7-tray-trial.ps1`)

One release client with `--trial-tray --no-activate --simulate-audio` on an isolated
invisible desktop. App-window visibility sampled every 100 ms (V = shown, h = hidden).

| Check | Software | OpenGL (FemtoVG) |
|---|---|---|
| Tray message window created / icon shown | yes / no (no shell on that desktop) | yes / no |
| Visibility trace | `VVVVVVVVVVVVVhhhhhhhhhhhhhhVVVVVVVVVVVVVVh` | `hVVVVVVVVVVVVVhhhhhhhhhhhhhhVVVVVVVVVVVVVVhh` |
| `closed visible=false` → process keeps running | PASS | PASS |
| Restore through the tray window's message path | PASS (`restored visible=true`) | PASS |
| Exit through the tray menu command, `TURBO_PANCAKE_EXIT`, exit code 0 | PASS | PASS |
| Foreground samples owned by the client | 0 | 0 |

The first run of the trial failed its own visibility check although the client's
markers were right: winit's message window (`Winit Thread Event Target`) is created
`WS_VISIBLE`, so "any visible window of the process" stayed true. The trial now counts
only app windows (`Window Class`). Earlier smoke scripts that report
`native_window_seen` use the looser check; their first-frame markers remain the
evidence that a real window was painted.

## Real notification-area icon

`cargo test -p turbo-pancake-desktop --lib real_icon -- --ignored --nocapture` on the
user's desktop: `Shell_NotifyIconW(NIM_ADD)` succeeded (`shown=true`) and the icon was
removed on drop. No window was shown; no focus or input. A click on the real icon and
the real context menu were **not** exercised (that needs the user's pointer):
owner-assisted interactive check.

## Startup smoke after the event-loop change

`desktop-smoke` (isolated desktop, release): software first frame 20 ms, OpenGL 161 ms,
clean exit markers and code 0 for both — `window.run()` was replaced by
`show()` + `run_event_loop_until_quit()`.
