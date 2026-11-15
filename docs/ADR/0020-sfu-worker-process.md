# ADR 0020 — The SFU can run as a worker process of the server

Status: accepted (2026-11-15). Spec §3 ("modular monolith plus an isolated SFU /
media-service"), §12 (supervised restart). Amends ADR 0007 (the SFU as a thread of
`app-server`).

## Context

The SFU ran as one thread inside `app-server`. Release builds abort on panic, and the
media path is the part of the server with the most third-party protocol code (ICE, DTLS,
SRTP, RTP) fed by the network: a fault there ended accounts, chats and presence for
everyone until systemd restarted the whole server (known issue 50).

## Decision

`media_sfu::remote`: the same `Sfu`, run by a worker process and driven over its standard
input and output.

* `RemoteSfu` (server side) has the interface of `Sfu`; `MediaSfu` is either of them, so
  the signaling code does not know the difference. Commands and the members' events are
  length-prefixed binary frames; the format is private to one build of the server (both
  ends are the same binary: `turbo-pancake-app-server sfu-worker`).
* Media over UDP never crosses the pipe: the worker owns the UDP socket. Tunnel
  datagrams (members without UDP) do.
* A supervisor thread pumps the pipes. When the worker ends it calls `PeerSink::closed`
  for every member that had a media session, clears the statistics and starts a new
  worker (at once after a long run, with a growing delay up to 5 s if it keeps dying).
  Clients notice the dead media session by their silence timeout (3 s) and reconnect
  their media as after a network failure; signaling, chats and call membership never
  left the server process.
* Off by default (tests, development, the one-process deployment keep working);
  `--media-process` / `TURBO_PANCAKE_MEDIA_PROCESS` turns it on, `install-server.sh`
  sets it. `GET /v1/media-stats` reports `worker_starts`.

## Evidence

* `crates/media-sfu/tests/remote.rs`: two real client call stacks through the worker
  (every voice frame forwarded, statistics over the pipe); the worker is killed — both
  members are told, a new worker starts, new members connect and every frame is
  forwarded again.
* `apps/app-server/tests/signaling.rs::media_runs_in_a_worker_process`: the server binary
  as its own worker, a media offer answered through it.
* Unit tests of the frame format (round trip, refusal of broken frames).

## Not done

* The SFU is still one thread per worker; sharding rooms over several workers is the
  next step this boundary makes possible (known issue 25).
* The target-server run with `TURBO_PANCAKE_MEDIA_PROCESS=true` (Linux pipes, systemd
  limits counting the worker) is owner-assisted; the code path is the same on both
  systems but was run on Windows only.
* The server does not tell clients "media restarted": they find out by timeout.
