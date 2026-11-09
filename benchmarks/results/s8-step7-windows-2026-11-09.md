# S8 step 7 — release and administration (Windows dev machine, 2026-11-09)

Tools: `deploy/release.ps1`, `deploy/linux/publish-update.sh`,
`deploy/linux/target-bench.sh`; guide `docs/ADMIN.md`.

## Release build

Final build from the clean tree at `476a9f8` (version 0.1.2, a throw-away trial update
key; no server address or CA built in): `s8-step7-release-windows-2026-11-09.md` (file
list, sizes, SHA-256).

| Check | Result |
|---|---|
| Installer size | 13.3 MiB (13 904 384 bytes), limit 30 MB: PASS |
| `SHA256SUMS` self-check at the end of the build | 9 files match: PASS |
| `release.ps1 -Verify` on the output | PASS (exit 0) |
| `sha256sum -c SHA256SUMS` (Git's coreutils, Linux format) | PASS |
| Tampering: one changed byte in `server/install-server.sh` + one extra file | `mismatch …; unlisted extra.txt`, exit 1: PASS; restored → PASS |
| Signed update manifest verifies with the pinned key | PASS (inside the build) |
| Same manifest offered to a client already at 0.1.2 | refused "not newer than the installed one", exit 1: PASS |
| Update contains the client only (launcher replaced by the installer, ADR 0016) | 2 files in `update/` (manifest + client): PASS |
| Release version in the client (`TURBO_PANCAKE_BUILD_VERSION`) | `0.1.2` in the binary; the client now also reports it to the server |
| `src.tar.gz` = the commit | `git get-tar-commit-id` → `476a9f8…`; contains the new server scripts: PASS |
| Reproducibility | client and launcher identical (same SHA-256) in the trial build (dirty tree, same code) and the final build |
| Dirty tree refused without `-AllowDirty`; recorded as `"dirty": true` with it | PASS (trial build) |

The first trial run failed on a PowerShell parse error in the script itself
(`"$Version:"`), fixed before any build.

## Server scripts

* `publish-update.sh` exercised in Git's bash on fake releases 0.1.1 → 0.1.2 → 0.1.3:
  files in place before the manifest, manifest replaced by rename, versions kept = new +
  previous (0.1.1 removed), a directory with two versions refused (exit 2): PASS.
* `install-server.sh`, `target-bench.sh`, `backup.sh`, `restore.sh`, `publish-update.sh`:
  `bash -n` syntax check PASS. Execution on Linux: **UNVERIFIED (owner-assisted)**.

## Not verified here

* Running `install-server.sh` with the bundle, `target-bench.sh` steps 1–5 and `r` on the
  target server, publishing to a live server and a client updating from it there, the
  real uplink — owner-assisted (ADMIN.md §9).
* Installing the release installer from this run: not repeated; the same installer code
  passed `s7-install-trial.ps1` / `s7-e2e-trial.ps1` (S7).
* Authenticode signing and the release update key: owner tasks.
