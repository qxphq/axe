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

## 0. The mode

The review is read-only by this contract. Explore mode adds the kernel to that
— the same guarantee enforced instead of promised — and is the better place for
a review, but it is not required: start where the session already is, and never
ask the user to run `/explore` for one. In normal mode `write`, `edit` and every
command that would change the tree are still off the table for the whole
review; a wider tool array is not permission. The report says which of the two
held it read-only.

`fetch` and `web_search` exist only in explore mode: a check that needs them is
named in the report, not replaced with a shell command. A check that needs a
live run which writes something (a build, a test, a binary that keeps state) is
named as the exact `!cmd` for the user's shell; one that writes only under
`$TMPDIR` may run. Never switch modes mid-review.

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
- Do not modify any file. Read, run only commands that change nothing in the
  tree, and use `fetch` or `web_search` where the brief needs them and the
  session's mode has them.
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
- If a claim needs a measurement or a run the review cannot make (it would
  write something, or the mode does not offer the tool), say so and give the
  exact `!` command; never guess a number.

## 3. Stop for triage

Present the report and wait. Use its Fix readiness verdict to triage:
`objective` findings can be applied as described; `needs input` findings wait
for the user's answer. Do not fix findings, tick the checklist, or commit until
the user asks.

## 4. Close out after confirmation

Only after the user asks. Writing is the fix phase's business: if the latest
mode note says explore mode is on, say `/explore off` and wait — the review
itself never asked for that switch.

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
