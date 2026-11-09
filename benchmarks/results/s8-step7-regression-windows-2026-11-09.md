# Regression suite (PASS)

Commit `476a9f8` (plus uncommitted changes, if any), 20261109T151207Z, Windows, 20 logical CPUs, renderer `winit-software`. Total 11.1 min: 16 PASS, 0 FAIL, 0 SKIP.

| Group | Item | Result | Time | Details |
|---|---|---|---|---|
| checks | fmt | **PASS** | 2.5 s |  |
| checks | clippy | **PASS** | 2.6 s |  |
| checks | tests | **PASS** | 208.5 s | 209 passed, 0 failed, 4 ignored, 1 filtered, 54 binaries |
| build | release | **PASS** | 124.9 s |  |
| load | messenger-100u | **PASS** | 39.1 s | presence missed=0 fanout missed=0 stale=0 |
| load | group-load-1x100 | **PASS** | 7.9 s | sends=400 missed=0 while_removed=0 storm_missed=0 gaps=0 |
| load | sfu-10m-accounts | **PASS** | 18.2 s | delivered t1=1.006 t10=1.000 |
| load | screen-publishers-10 | **PASS** | 10.9 s | main=0.992 preview=0.992 voice=1.001 high=2 low=0 cpu=7% |
| load | screen-grid-10 | **PASS** | 10.9 s | main=1.0 preview=1.001 voice=1.0 high=0 low=0 cpu=6% |
| load | screen-fanout-10-budget | **PASS** | 10.7 s | main=0.997 preview=1.0 voice=1.000 high=4 low=6 cpu=5% |
| load | login-burst-100 | **PASS** | 3.8 s | ok=83 503=17 peak_hashes=4 mem_above_idle=81MiB |
| native | tray | **PASS** | 5.4 s | checks: 6 true, 0 false |
| native | instance | **PASS** | 9.6 s | checks: 6 true, 0 false |
| native | layout | **PASS** | 52.6 s | checks: 10 true, 0 false |
| native | s6-chat-call | **PASS** | 65 s | S6 native trial PASS; backend=winit-software; TLS/WSS; simulated audio; isolated invisible desktop; no foreground/input/capture |
| native | reconnect | **PASS** | 92.2 s | S7 reconnect trial PASS; backend=winit-software; outage_ms=8000; restored_after_restart_ms=565; TLS/WSS; simulated audio; invisible desktop; foreground_samples=... |

Logs: one file per item in the local log directory (not committed).
