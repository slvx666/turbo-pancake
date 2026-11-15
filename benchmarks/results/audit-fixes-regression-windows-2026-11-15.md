# Regression suite (PASS)

Commit `53b92ee` (plus uncommitted changes, if any), 20261115T193050Z, Windows, 20 logical CPUs, renderer `winit-software`, full. Total 26.9 min: 19 PASS, 0 FAIL, 0 SKIP.

| Group | Item | Result | Time | Details |
|---|---|---|---|---|
| checks | fmt | **PASS** | 2.7 s |  |
| checks | clippy | **PASS** | 7.6 s |  |
| checks | tests | **PASS** | 239.8 s | 222 passed, 0 failed, 4 ignored, 1 filtered, 56 binaries |
| build | release | **PASS** | 147.6 s |  |
| load | messenger-100u | **PASS** | 40.2 s | presence missed=0 fanout missed=0 stale=0 |
| load | group-load-1x100 | **PASS** | 7.9 s | sends=400 missed=0 while_removed=0 storm_missed=0 gaps=0 |
| load | sfu-10m-accounts | **PASS** | 18.1 s | delivered t1=1.000 t10=1.001 |
| load | screen-publishers-10 | **PASS** | 10.5 s | main=0.993 preview=1.008 voice=1.004 high=2 low=0 cpu=7% |
| load | screen-grid-10 | **PASS** | 10.7 s | main=1.0 preview=1.006 voice=1.0 high=0 low=0 cpu=11% |
| load | screen-fanout-10-budget | **PASS** | 10.7 s | main=0.997 preview=1.0 voice=1.002 high=4 low=6 cpu=9% |
| load | login-burst-100 | **PASS** | 4.2 s | ok=80 503=20 peak_hashes=4 mem_above_idle=82MiB |
| native | tray | **PASS** | 5.5 s | checks: 8 true, 0 false |
| native | instance | **PASS** | 10 s | checks: 6 true, 0 false |
| native | layout | **PASS** | 51.2 s | checks: 10 true, 0 false |
| native | s6-chat-call | **PASS** | 64.9 s | S6 native trial PASS; backend=winit-software; TLS/WSS; simulated audio; isolated invisible desktop; no foreground/input/capture |
| native | reconnect | **PASS** | 95.3 s | S7 reconnect trial PASS; backend=winit-software; outage_ms=8000; restored_after_restart_ms=559; TLS/WSS; simulated audio; invisible desktop; foreground_samples=... |
| native | update | **PASS** | 396.3 s | S7 update trial PASS; backend=winit-software; TLS; isolated invisible desktop; foreground_samples=0 |
| native | installer | **PASS** | 157.3 s | S7 install trial PASS; backend=winit-software; isolated user folders; trial id s7i-8df85fb1bc; invisible desktop; foreground_samples=0 |
| native | e2e | **PASS** | 333.2 s | S7 E2E trial PASS; backend=winit-software; trial id s7e322bc59b10; isolated user folders; invisible desktop; simulated audio; foreground_samples=0 |

Logs: one file per item in the local log directory (not committed).
