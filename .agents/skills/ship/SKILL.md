---
name: ship
description: Wrap up a finished change in axe. Use when the user says ship, wrap up, or finish: format, lint, test, sync the flake lock and docs, then commit.
---

# ship

Run from the repo root, in order. Stop at the first failure and report it;
do not fix speculatively.

1. `cargo fmt`, then `cargo clippy --all-targets -- -D warnings`, then
   `cargo test --locked`. CI runs exactly these.
2. If `flake.nix` exists and `nix` is on PATH: `nix flake lock`. It adds a
   missing input node and leaves existing ones alone; never `nix flake
   update` during a wrap-up. Stage `flake.lock` only if it changed. A
   flake with `inputs` and no committed `flake.lock` is broken for every
   consumer pinned to a rev (`cannot write modified lock file of flake`).
3. Update `README.md` if user-facing behavior changed and `AGENTS.md` if an
   invariant changed. Same commit; they are part of the change.
4. `git status --short`. Stage only files the change named; never `target/`
   or `references/`.
5. One commit per feature, imperative subject, no prefix. Do not push.
6. Report the commit hash, subject, files touched, and anything skipped.
