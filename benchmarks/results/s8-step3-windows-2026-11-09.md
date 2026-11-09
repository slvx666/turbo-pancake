# S8 step 3 — media load with accounts (Windows dev machine, loopback)

Date: 2026-11-09. Tools: `turbo-pancake-bench sfu-screen` and `sfu` with `--accounts`
(`benchmarks/harness/src/{sfu_screen,sfu,call_accounts}.rs`). Builds: release server
and bench from `7d0e25d` plus this step's changes. Raw reports (`benchmarks/results/`):
`sfu-screen-publishers-50-accounts-windows-1794229771.json`,
`sfu-screen-grid-50-accounts-windows-1794229788.json`,
`sfu-screen-fanout-50-accounts-windows-1794229806.json`,
`sfu-screen-fanout-50-accounts-budget40-windows-1794229823.json`,
`sfu-screen-publishers-50-accounts-budget2-windows-1794230012.json`,
`sfu-screen-fanout-50-accounts-{8000k,16000k,24000k}-windows-17942298{41,58,75}.json`,
`sfu-50m-accounts-windows-1794229918.json`; anonymous re-runs
`sfu-screen-{publishers,grid,fanout}-50-windows-17942299{33,48,63}.json`.

## What runs

* **Accounts mode**: the release server with real PostgreSQL (`crates/pg-test`,
  `fsync=off`) and `--load-test-limits` (loopback only). All members are registered
  accounts in one group; member 0 starts the group call (`POST /v1/calls`), the others
  join it (`POST /v1/calls/{id}/join`), and each signed-in WebSocket attaches its media
  with `JoinCallMedia` (participation lease, ADR 0015). Offers, shares and subscriptions
  name the call's media room the server assigned. Anonymous rooms are refused to signed-in
  connections, so this is the path real clients use.
* Real call drivers (ICE + DTLS-SRTP over loopback UDP) in the bench process; the server
  is its own process. Synthetic H.264-shaped access units (IDR each), high 2 Mbit/s 30 fps
  and preview 150 kbit/s 10 fps unless noted; the share announces its bitrate (the high
  budget counts it).
* `--talkers 3`: members 0–2 also send voice (100 B every 20 ms ≈ Opus 40 kbit/s) during
  the share, received by every other member.
* `--max-high-egress-mbps B`: the server's high-layer budget. Viewers that do not fit get
  the preview layer on their main picture and `ScreenSubscription{layer: LOW, reason:
  ADMISSION}`; the report checks every viewer against the layer it was told.
* `pass`: every subscription got ≥ 98 % of the packets of the layer it was told it gets
  (sender counters refresh every 0.5 s, so window edges cost up to ~1 %), voice ≥ 98 %,
  every viewer told a layer, and with a budget exactly ⌊B / high⌋ viewers on high and all
  others on low with reason `ADMISSION`.
* CPU: `GetProcessTimes` (fixed in step 2). Loopback: server cost on this machine
  (20 logical CPUs), not the network or the owner's uplink.

## Results (accounts, 10 s each)

| Scenario | Pass | Delivered main / preview / voice | Server CPU (one core) | Out packets/s | Egress est. | Server WS |
|---|---|---|---|---|---|---|
| 50 publishers + 2 viewers (each 1 high + 49 previews) | **PASS** | 99.9 / 100.0 / 100 % | 27 % | 10 100 | 30 Mbit/s | 49 MiB |
| 50 members × 49 previews (grid, 2450 streams) | **PASS** | — / 99.4 / 99.8 % | 66 % | 56 400 | 403 Mbit/s | 283 MiB |
| 1 → 50 high viewers | **PASS** | 99.5 / — / 100 % | 31 % | 19 500 | 116 Mbit/s | 96 MiB |
| 1 → 50 at 8 Mbit/s 60 fps | **PASS** | 99.4 / — / 100 % | 49 % | 55 500 | 436 Mbit/s | 117 MiB |
| 1 → 50 at "16" / "24" Mbit/s 60 fps (sender capped at 12) | **PASS** | 99.6 / 100.4 % | 60 / 64 % | 77 100 / 76 400 | 647 / 646 Mbit/s | 121 / 120 MiB |
| Voice only, 50 members: 1 / 10 / 50 talkers | 99.9 / 100.0 / 100.1 % | — | 9.5 / 33 / 92 % | 2 400 / 24 500 / 122 600 | 3 / 33 / 163 Mbit/s | — |

Setup with accounts: 52 registrations 0.25 s (8 in parallel), call create 3–5 ms, join p50
2.7 ms / max 9 ms.

### Safe degradation with the high budget

| Run | High viewers (told HIGH) | Low (told LOW, reason ADMISSION) | Main picture received | Voice | Previews |
|---|---|---|---|---|---|
| 1 → 50, budget 40 Mbit/s (fits 20 × 2 Mbit/s) | **20** | **30 / 30** | high 2001 kbit/s, low 150 kbit/s; 99.3 % of the told layer | 100 % | — |
| 50 publishers + 2 viewers, budget 2 Mbit/s (fits 1) | **1** | **1 / 1** | high 2000, low 150 kbit/s; 99.5 % | 100 % | 99.5 % of 98 streams |

Order of what is kept when the budget runs out (spec §5.3): voice is not counted against
the budget and kept 100 %; the watched high stream goes to the earliest subscribers up
to the budget; everybody else keeps a picture on the preview layer and is told why;
previews keep flowing. Egress with the 40 Mbit/s budget: 57 Mbit/s total (40 high + 30 ×
0.15 preview + voice + headers) instead of 116 Mbit/s.

### Measured limit of this machine

* The highest load reached: 50 viewers × 12 Mbit/s = **647 Mbit/s egress, 77 000
  packets/s, 60–64 % of one core**, ≥ 99.5 % delivered. The server was not saturated.
  One share is capped at 12 Mbit/s by the sending client's pacer
  (`CallConfig::video_pacing_bps` default), so "16" and "24" Mbit/s runs send 12.
* The heaviest CPU per packet is the grid (2450 forwarded streams): 66 % of a core at
  56 000 packets/s; voice with 50 talkers (2450 voice streams, 122 600 packets/s): 92 %.
  At ≈ 8–12 µs per forwarded packet the single SFU thread tops out near 100 000–125 000
  packets/s on this CPU (issue 25: single-threaded SFU). On the target server's 1 Gbit/s
  uplink the network budget (≈ 1 Gbit/s ≈ 50 × 20 Mbit/s, spec §5.3) is reached before
  that; that run is owner-assisted.

### Accounts vs anonymous; corrected S4 figures

Anonymous re-runs (no voice, same binaries): publishers 21 %, grid 60 %, fanout 23 % of a
core, all PASS. The account path adds no measurable per-packet cost (accounts runs carry
3 talkers' voice in addition). The S4 report gave publishers 23 %, fanout 24 % and grid
**92 %** of a core: those were read with the unreliable sysinfo probe (step 2 finding);
the grid figure was overstated, the conclusion "single SFU thread is the CPU limit"
stands at the higher packet rates above.

## Not covered here (UNVERIFIED / owner-assisted)

* The target server (its CPU, real 1 Gbit/s uplink, durable database).
* Network loss or congestion on the path (loopback); the network policy layer choice is
  covered by S4 unit/netem tests, not re-run here.
* Real encoders/decoders (synthetic payloads): S3/S4 cover them.
