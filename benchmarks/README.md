# Benchmarks

`turbo-pancake-bench` (`benchmarks/harness`) writes one JSON report per run into
`benchmarks/results/` containing host info (OS version, CPU, RAM, git commit), parameters,
raw samples and a summary (min/p50/p95/p99/max). Reports are committed so that numbers
in ADRs and `docs/PROJECT_STATE.md` can be traced to evidence.

## Scenarios

### `desktop-startup`

Spawns the release client with `--smoke-exit-after-ms`, measures spawn → first painted
frame (client prints `TURBO_PANCAKE_FIRST_FRAME` from a winit `about_to_wait` hook after the first
`RedrawRequested`), waits `--settle-ms`, samples memory of the process, verifies clean exit.

* Memory: Windows working set / peak / private bytes (`GetProcessMemoryInfo`); Linux
  VmRSS / VmHWM / Private_* from `smaps_rollup`; macOS RSS only (private: TODO).
* GPU memory is **not** sampled yet (spec §13 requires it separately) — TODO S7.
* Run 0 after a build is the closest automated approximation of a cold start; a true cold
  start (flushed file cache / after reboot) is a manual procedure.
* `--env SLINT_BACKEND=winit-software` selects the software renderer.
* `--client-arg` forwards flags, e.g. `--window-pos 2020,100 --no-activate`.

### `signaling`

Starts the release server on an ephemeral loopback port, then measures:
relay latency A→server→B for `--messages` ICE signals (same-process clock), Ping/Pong
RTT, server memory at start / with 2 peers / with `--idle-connections` additional
authenticated-less idle WebSockets, and RTT while those are attached.

Loopback numbers measure server overhead only, not network latency.

### `sfu`

Starts the release server on loopback and connects `--members` (default 20, ≤ 64) media
members: real signaling (hello, join, `MediaOffer` over UDP, `TrackPublished` →
`expect_source`) and a real `CallDriver` each (ICE + DTLS-SRTP), no audio engine. For each
value of `--talkers` (default `1,3,10,20`) that many members send a `--payload`-byte
frame (default 100 B ≈ Opus voice at 40 kbit/s) every 20 ms for `--seconds`; reports the
server's CPU (process CPU time / wall, % of one core), packets in/out per second, the share
of expected packets delivered, members receiving, and estimated egress (payload + 66 B
IPv4/UDP/RTP/extensions/SRTP-tag per packet). The server is a separate process, so the
members' own work is not in its CPU figure; members share the machine, so a saturated run
measures the box, not only the server.

```bash
cargo build --release -p turbo-pancake-app-server -p turbo-pancake-bench
turbo-pancake-bench sfu --server-exe <target>/release/turbo-pancake-app-server --members 20
```

### `media-probe` (tools/media-probe)

JSON dump of audio hosts/devices/default configs plus a 1 s open test of the default input
and output (output writes silence).

Reports dated before the 2026-11-02 rename reference the old `dsmini-*` executable names.

## Result index (Windows dev machine, see JSON for details)

| File | Summary |
|---|---|
| `desktop-startup-femtovg-windows-1793574002.json` | first frame p50 398 ms; WS 71.5 MiB; private 76.7 MiB; exe 9.1 MiB |
| `desktop-startup-software-windows-1793574034.json` | first frame p50 73 ms; WS 21.8 MiB; private 6.2 MiB |
| `signaling-before-ws-buffer-tuning-windows-1793574040.json` | relay p50 76 µs / p99 149 µs; 140 KB per idle WS (128 KiB default buffers) |
| `signaling-windows-1793574147.json` | relay p50 72 µs / p99 153 µs; 17.9 KB per idle WS; 500 idle → 14.3 MiB total |
| `media-probe-windows-*.json` | WASAPI, 3 in / 3 out, default mic 192 kHz mono, open test OK |
| `sfu-20m-windows-1793711275.json` | 20 members; talkers 1 / 3 / 10 / 20 → out 951 / 2853 / 9425 / 18991 pps, server CPU 6.9 / 6.2 / 14.4 / 25.4 % of one core, delivered 99–100 %, egress ≈ 1.3 / 3.8 / 12.5 / 25.2 Mbit/s |
| `sfu-50m-windows-1793711325.json` | 50 members; talkers 3 / 10 / 25 / 50 → out 7.3k / 24.5k / 61.3k / 71.8k pps, CPU 16 / 31 / 81 / **97 %** of one core, delivered 100 / 100 / 100 / **59 %** (SFU thread saturated, degrades by dropping); ≈ 13 µs per forwarded packet under load |
| `group-trial-windows-20261103.log` | three release clients on real devices (one machine), 60 s: all connected, 0 concealed / 0 late / 0 decode errors, voice-activated client sent 0 frames in a quiet room |
