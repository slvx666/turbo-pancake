# turbo-pancake — instructions for every session

Native desktop messenger (Rust workspace: Slint UI, Axum server, str0m WebRTC).
Talk to the owner in Russian.

## Start of every session ("продолжай" means exactly this)

1. Read the spec in full: `C:\Users\burger\Downloads\Telegram Desktop\TECHNICAL_SPEC_MESSENGER_RU.md`
   (v1.0, kept outside the repo on purpose: the repo is public). If the file is missing, ask
   the owner for it before planning anything.
2. Read `docs/PROJECT_STATE.md` — current stage, acceptance status, blockers and the
   **"Exact next task"** section. Continue from there without re-planning.
3. Skim `docs/STAGES.md`, the ADRs in `docs/ADR/`, `git status` and `git log --oneline -10`.
4. Work on the first unfinished stage only (spec §16–§17). Never mark a stage DONE without
   PASS evidence for every acceptance criterion; anything unchecked is `UNVERIFIED`.
5. Before ending: run fmt / clippy / tests, update `docs/PROJECT_STATE.md` (status per
   criterion, evidence, issues, exact next task), commit, push, check CI.

## Hard rules on this machine

* **GUI tests: secondary monitor only, never take the mouse or focus.** The owner keeps
  working while tests run. Launch the client with `--window-pos 2020,100 --no-activate`;
  smoke-check with `benchmarks/scripts/window-check.ps1`. Never move the mouse or send
  input; screenshot only our own window and do not commit screenshots that show other apps.
* **Drive C: is almost full.** The build goes to `D:/turbo-pancake-target` via the gitignored
  `.cargo/config.toml` (`[build]` / `target-dir = "D:/turbo-pancake-target"`); recreate it if it
  is missing. Do not put large files on C:.
* Rust 1.99.1 is in `%USERPROFILE%\.cargo\bin`; in Bash run
  `export PATH="$HOME/.cargo/bin:$PATH"` before cargo.
* Git: `origin` = https://github.com/slvx666/turbo-pancake (public), branch `main`.
  The owner allowed committing and pushing to it. `gh` is logged in as `slvx666`;
  check CI with `gh run list -R slvx666/turbo-pancake` / `gh run watch <id>`.
* Do not copy the spec or other private material into the repo.

## Checks

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked
```

Benchmarks and manual procedures: `docs/TESTING.md`, `benchmarks/README.md`.
