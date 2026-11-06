# ADR 0016 — Signed updates with staged install and rollback (Windows)

Status: accepted (2026-11-06, S7 step 5). Spec §9 (signed updates, staged/atomic,
rollback, no unverified exe over HTTP), §12, §14.5 (IP-only TLS + updater).

## Context

The client must find, download and install new versions by itself, without trusting
the transport alone: the server is reached by IP with a private CA (ADR 0014), and a
compromised or impersonated server must not be able to push code. Windows Authenticode
signing needs the owner's certificate (owner decision, S7 step 6) and is not enough by
itself for rollback/downgrade protection. A broken update must not leave the user
without a working client.

## Decision

**Signed manifest.** A release is described by a JSON manifest, signed with **Ed25519**.
The server serves an *envelope*
`{"key_id", "manifest": base64(manifest bytes), "signature": base64(sig)}`; the
signature covers the exact manifest bytes (no canonicalisation). The manifest (format 1,
unknown fields rejected) holds `product`, `platform` (`windows-x86_64`), `version`
(`major.minor.patch`), `published_at_ms`, `expires_at_ms`, optional `notes` and the
list of `files` (`path`, `size`, `sha256`), one of which is the client
`turbo-pancake-desktop.exe`.

**Pinned keys.** Trusted public keys are compiled into the client
(`TURBO_PANCAKE_UPDATE_PUBKEYS="<id>:<base64>,…"` at build time). A build without keys
reports "обновления не настроены" and never installs anything. Several ids allow key
rotation: a new client version ships the new key before the old one is retired. The
private key is created with `turbo-pancake-update keygen` and stays with the owner
outside the repository and outside the server; the server only stores the signed
envelope and the files. Creating and keeping the release key is an owner task.

**Checks before anything is installed** (`crates/update`): envelope ≤ 64 KiB; known
key id; valid signature; product/platform match; not expired (freeze protection) and
not published more than 10 min in the future; version strictly greater than the running
one (no downgrade/replay) and not on the local "bad" list; ≤ 32 files, ≤ 256 MiB in
total; paths relative, `[A-Za-z0-9._-]` segments, no duplicates. Each file is streamed
over HTTPS from the same server (`/v1/update/files/<version>/<path>`, never a URL from
the manifest), with the declared size as a hard limit and SHA-256 checked before use.

**Install layout (per user, no admin rights):**

```text
<root>/turbo-pancake.exe          launcher (stable; replaced only by the installer)
<root>/state.json                 {current, previous, pending:{version, attempts}, bad}
<root>/versions/<version>/…       one directory per installed version
<root>/staging/<version>/…        download in progress (never started from)
```

`state.json` is written to a temporary file, flushed and renamed over the old one
(atomic replace on NTFS), so every crash leaves either the old or the new state. A
finished, verified staging directory is renamed into `versions/` and only then recorded
as `pending`.

**Switch and rollback.** The client asks the user (settings panel) and restarts through
the launcher (`--wait-pid` lets the old process release its single-instance lock). The
launcher starts a pending version as a *trial* (attempts = 1) and waits up to 60 s for
confirmation. The new client confirms after its first frame plus a few seconds of
running (current ← new, previous ← old, pending cleared; older versions removed). If it
exits without confirming, or the launcher finds an already-attempted pending version at
its next start (e.g. a crash or power loss during the trial), the version is marked bad,
pending is cleared and the previous version starts. A bad version is never offered again.

**Server.** `GET /v1/update/manifest` returns the stored envelope (404 when none is
published); `GET /v1/update/files/{version}/{*path}` serves files from the update
directory (`--update-dir`), with the same path rules and a limit on concurrent
downloads. No authentication: signed-out clients and the installer need it; integrity
comes from the signature, not from the server.

**Cryptography** comes from `aws-lc-rs`, already in the workspace for TLS and DTLS
(Ed25519 sign/verify, SHA-256); no new crypto dependency.

## Consequences

* Release flow: build → `turbo-pancake-update sign` (offline, owner's key) → copy the
  output directory to the server's `--update-dir`. Expiry (default 90 days) means a
  release must be re-signed periodically, otherwise clients stop seeing it.
* The launcher itself is not updated by this mechanism (installer, step 6).
* Authenticode (owner's certificate) is complementary and recorded as owner-assisted.
* macOS/Linux packaging and their update paths belong to S9.
