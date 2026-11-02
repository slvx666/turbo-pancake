# turbo-pancake — instructions for every session

Native desktop messenger (Rust workspace: Slint UI, Axum server, str0m WebRTC).
Talk to the owner in Russian.

## Start of every session ("продолжай" means exactly this)

1. Read the spec in full: `TECHNICAL_SPEC_MESSENGER_RU.md` v1.0. It is kept outside the repo
   on purpose (the repo is public); its local path is in the untracked `CLAUDE.local.md`.
   If the file or that note is missing, ask the owner before planning anything.
2. Read `docs/PROJECT_STATE.md` — current stage, acceptance status, blockers and the
   **"Exact next task"** section. Continue from there without re-planning.
3. Skim `docs/STAGES.md`, the ADRs in `docs/ADR/`, `git status` and `git log --oneline -10`.
4. Work on the first unfinished stage only (spec §16–§17). Never mark a stage DONE without
   PASS evidence for every acceptance criterion; anything unchecked is `UNVERIFIED`.
5. Before ending: run fmt / clippy / tests, update `docs/PROJECT_STATE.md` (status per
   criterion, evidence, issues, exact next task), commit, push, check CI.

## Rules

* The repo is **public**: never commit IP addresses, personal paths or names, details of
  the owner's machine, network or testers, or the spec text. Describe trials generically
  ("server on the tester's public IP").
* **Machine-specific rules** (where GUI tests may run, build directory, toolchain path,
  GitHub access) are in the untracked `CLAUDE.local.md`; read and follow it. If it is
  missing, ask the owner.

## Checks

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked
```

Benchmarks and manual procedures: `docs/TESTING.md`, `benchmarks/README.md`.
