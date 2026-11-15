# Regression suite (PASS)

Commit `fdad3ec` (plus uncommitted changes, if any), 20261115T203352Z, Windows, 20 logical CPUs, renderer `winit-software`, full. Total 24.7 min: 19 PASS, 0 FAIL, 0 SKIP.

| Group | Item | Result | Time | Details |
|---|---|---|---|---|
| checks | fmt | **PASS** | 2.6 s |  |
| checks | clippy | **PASS** | 13.1 s |  |
| checks | tests | **PASS** | 285.8 s | 235 passed, 0 failed, 4 ignored, 1 filtered, 59 binaries |
| build | release | **PASS** | 104 s |  |
| load | messenger-100u | **PASS** | 39.4 s | presence missed=0 fanout missed=0 stale=0 |
| load | group-load-1x100 | **PASS** | 7.3 s | sends=400 missed=0 while_removed=0 storm_missed=0 gaps=0 |
| load | sfu-10m-accounts | **PASS** | 17.7 s | delivered t1=0.999 t10=0.999 |
| load | screen-publishers-10 | **PASS** | 10 s | main=0.992 preview=0.994 voice=0.996 high=2 low=0 cpu=6% |
| load | screen-grid-10 | **PASS** | 10.3 s | main=1.0 preview=0.999 voice=1.0 high=0 low=0 cpu=3% |
| load | screen-fanout-10-budget | **PASS** | 10.2 s | main=1.000 preview=1.0 voice=1.002 high=4 low=6 cpu=4% |
| load | login-burst-100 | **PASS** | 3.5 s | ok=83 503=17 peak_hashes=4 mem_above_idle=81MiB |
| native | tray | **PASS** | 5.4 s | checks: 8 true, 0 false |
| native | instance | **PASS** | 9.8 s | checks: 6 true, 0 false |
| native | layout | **PASS** | 50.6 s | checks: 10 true, 0 false |
| native | s6-chat-call | **PASS** | 64.8 s | S6 native trial PASS; backend=winit-software; TLS/WSS; simulated audio; isolated invisible desktop; no foreground/input/capture |
| native | reconnect | **PASS** | 81.9 s | S7 reconnect trial PASS; backend=winit-software; outage_ms=8000; restored_after_restart_ms=548; TLS/WSS; simulated audio; invisible desktop; foreground_samples=... |
| native | update | **PASS** | 357 s | S7 update trial PASS; backend=winit-software; TLS; isolated invisible desktop; foreground_samples=0 |
| native | installer | **PASS** | 151.4 s | S7 install trial PASS; backend=winit-software; isolated user folders; trial id s7i-e800354929; invisible desktop; foreground_samples=0 |
| native | e2e | **PASS** | 254.5 s | S7 E2E trial PASS; backend=winit-software; trial id s7e66dfbb7bb0; isolated user folders; invisible desktop; simulated audio; foreground_samples=0 |

Logs: one file per item in the local log directory (not committed).
