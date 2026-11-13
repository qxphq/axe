---
name: next-review
description: Use when the user asks to run the next unchecked review in axe's review pipeline. Triggered by "next review", "run the next review", "continue reviews", "continue with the next step in reviews", or "next review in the pipeline".
---

# Run the next review in the pipeline

Paths below are relative to the repository root, not to this skill's
directory.

`reviews/` holds one concern-scoped brief per review plus `progress.md`, the
queue. The common review contract lives here; briefs define only their scope
and checks, so do not reintroduce it into a brief. The review is read-only;
fixes happen only after the user triages the report.

## 0. Confirm the mode

The review runs in explore mode, so the kernel — not this contract — is what
makes it read-only. Confirm `write` and `edit` are unavailable and `fetch` and
`web_search` are available. If they are not, stop and ask the user to run
`/explore`; do not start the review in normal mode.

A check that needs a live run outside the sandbox is not something to work
around. Name the exact command in the report and let the user run it as `!cmd`
— their shell, not the review's. Never leave explore mode mid-review.

## 1. Pick the review

Read `reviews/progress.md`. The next review is the first entry with `[ ]`,
top to bottom.

If every entry is checked, say the round is complete and stop; start a new
round only when the user asks.

Read the linked brief in full. It defines the scope and required checks for
that review.

## 2. Review

Follow the brief exactly. Defaults for every review:

- You have the whole repo. `README.md` ships through `about` and is the
  user-facing contract; `AGENTS.md` records the invariants a change must not
  break. Treat both as the map, but verify the code; a doc/code mismatch is
  itself a finding, so say which side is wrong.
- Do not modify any file. Read, run allowlisted read-only commands, and use
  `fetch` or `web_search` where the brief needs them.
- Every finding needs `file:line` (both sides where applicable), the invariant
  or contract it violates, and a concrete failure scenario (inputs → wrong
  outcome, or what breaks or degrades). No style nits and no speculation; drop
  a concern that is demonstrably handled elsewhere.
- For every required check, state whether it holds everywhere or where it
  breaks.
- Report findings by severity (critical / major / minor), most severe first,
  then the checks verified clean.
- End with a **Fix readiness** section: per finding, the concrete fix (what
  changes, where) and whether it is objectively determined by the docs and code
  (`objective`) or needs a decision from the user (`needs input`, with the exact
  question). Close with one verdict line: `All findings objectively fixable —
  no input needed.` or `Input needed — N finding(s); questions above.`
  Describe fixes, no patch code.
- If a claim needs a measurement or a run the sandbox refuses, say so and give
  the exact `!` command; never guess a number.

## 3. Stop for triage

Present the report and wait. Use its Fix readiness verdict to triage:
`objective` findings can be applied as described; `needs input` findings wait
for the user's answer. Do not fix findings, tick the checklist, or commit until
the user asks.

## 4. Close out after confirmation

Only after the user asks, and only once the latest mode note says explore mode
is off:

1. Implement the confirmed findings only; do not invent speculative changes.
2. Add a regression test for every behavioral fix, at the layer that owns it:
   a unit test when the logic is pure, a `harness.py` scenario when only the
   terminal or the sandbox shows it.
3. Run the gate the change needs: `cargo fmt --check`, `cargo clippy
   --all-targets -- -D warnings`, `cargo test --locked`, plus the relevant
   `./harness.py` scenarios when the change touches the screen, tools, sandbox,
   session resume, compaction, or MCP.
4. Tick the review's `[x]` entry in `reviews/progress.md` only after the fixes
   and checks pass.
5. Use the `ship` skill for the version bump, flake lock, docs sync, and
   commit. Do not duplicate its steps. The commit subject is imperative and
   names the concern, matching the repo's history: `Close <concern> review
   findings`.
6. If any question is still unanswered, do not tick, do not commit; report
   what remains and wait.
