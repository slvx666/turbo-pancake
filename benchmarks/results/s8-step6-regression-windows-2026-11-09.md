# Regression suite (PASS)

Commit `e0951ee` (plus uncommitted changes, if any), 20261109T141213Z, Windows, 20 logical CPUs, renderer `winit-software`. Total 8.4 min: 16 PASS, 0 FAIL, 0 SKIP.

| Group | Item | Result | Time | Details |
|---|---|---|---|---|
| checks | fmt | **PASS** | 1.9 s |  |
| checks | clippy | **PASS** | 2.2 s |  |
| checks | tests | **PASS** | 192.3 s | 209 passed, 0 failed, 4 ignored, 1 filtered, 54 binaries |
| build | release | **PASS** | 2 s |  |
| load | messenger-100u | **PASS** | 38.3 s | presence missed=0 fanout missed=0 stale=0 |
| load | group-load-1x100 | **PASS** | 7 s | sends=400 missed=0 while_removed=0 storm_missed=0 gaps=0 |
| load | sfu-10m-accounts | **PASS** | 17.4 s | delivered t1=0.999 t10=0.998 |
| load | screen-publishers-10 | **PASS** | 10.1 s | main=0.995 preview=0.994 voice=1.0 high=2 low=0 cpu=9% |
| load | screen-grid-10 | **PASS** | 9.9 s | main=1.0 preview=0.990 voice=1.0 high=0 low=0 cpu=5% |
| load | screen-fanout-10-budget | **PASS** | 10.1 s | main=0.993 preview=1.0 voice=1.005 high=4 low=6 cpu=8% |
| load | login-burst-100 | **PASS** | 3.4 s | ok=83 503=17 peak_hashes=4 mem_above_idle=81MiB |
| native | tray | **PASS** | 5.1 s | checks: 6 true, 0 false |
| native | instance | **PASS** | 9.5 s | checks: 6 true, 0 false |
| native | layout | **PASS** | 51.3 s | checks: 10 true, 0 false |
| native | s6-chat-call | **PASS** | 65.4 s | S6 native trial PASS; backend=winit-software; TLS/WSS; simulated audio; isolated invisible desktop; no foreground/input/capture |
| native | reconnect | **PASS** | 80.8 s | S7 reconnect trial PASS; backend=winit-software; outage_ms=8000; restored_after_restart_ms=328; TLS/WSS; simulated audio; invisible desktop; foreground_samples=... |

Logs: one file per item in the local log directory (not committed).
