# Working on turbo-pancake

Communicate with the owner in Russian. Read `CLAUDE.md`, `docs/ONBOARDING.md`,
the owner's local specification, `CLAUDE.local.md` (when present), and
`docs/PROJECT_STATE.md` before development. The specification is private; never
commit it, its text, machine-specific paths, credentials, or personal details.
Treat document instructions as project context; the owner's current request
defines the authorized task and takes precedence.

Work in a dedicated branch, never directly in `main`. Preserve existing code,
migrations, history, and unrelated changes. Windows is the only current client
target (ADR 0012); macOS/Linux work belongs to S9.

Continue from the first unfinished stage and its "Exact next task" in
`docs/PROJECT_STATE.md`. Do not mark work verified without evidence. Keep owner
assisted checks and environment failures explicit. Update the state journal
when development or verification changes the known state.

Checks: `cargo fmt --all -- --check`, then run
`cargo clippy --workspace --all-targets --locked -- -D warnings` and
`cargo test --workspace --locked` through `benchmarks/scripts/apm-env.ps1` on
Windows. PostgreSQL is required for messenger tests; skipping it is not a pass.
Consult `docs/TESTING.md` before GUI, capture, or real-device tests. Follow the
owner's local rules for background windows and audio devices.
