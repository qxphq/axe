# Test coverage and test quality

Axe review prompt — one concern: **the tests pin the invariants the docs
promise, the harness covers what unit tests cannot, and the gaps are known
rather than accidental.**

Read first: `AGENTS.md` "Build and test" and the invariants it protects; README
claims that a test could pin; `.github/workflows/ci.yml`; `harness.py` and
`mock.py` in full. Then read the tests in each module; if you need a live
test list, ask the user to run `!cargo test -- --list`, since the build writes
outside the review sandbox.

Verify with depth — map invariants to tests, do not count assertions:

- Invariant map: for each of `AGENTS.md`'s numbered invariants, name the test
  or harness scenario that would fail if it broke. Report each one with no
  test: prefix order and byte stability, append-only body and resume,
  compaction marker, frame layout and `slack`, one-writer repaint, editor keys,
  completion, markdown streaming, HTTP framing and SSE, tool caps and refusals,
  sandbox denials, skills discovery and precedence, config validation, MCP
  merge/trust/names/transports, web parsers, state and history, version and
  packaging.
- Distribution: the unit tests are concentrated in `main.rs`, `ui.rs`,
  `markdown.rs` and `web.rs`. List modules with few or no tests (`turn.rs`,
  `store.rs`, `sandbox.rs`, `prompt.rs`, `render.rs`) and say what a bug there
  would cost. Flag a test that only exercises a helper while the documented
  behavior lives in an untested caller.
- Quality, not presence: an assertion that mirrors the implementation, a test
  that would still pass if the feature broke, a happy path with no negative
  case (validation rejects, cleanup on failure, auth gates, cap boundaries,
  duplicate handling, cancellation). Check mocks and fixtures are deterministic:
  no real provider, no public DNS, no wall-clock or ordering assumptions, no
  fixed port collision, no dependence on `HOME`, `TERM` or locale.
- Unit-test hazards: tests that mutate process-wide state (`env::set_var`,
  `OnceLock`, cwd, `HOME`) and run in parallel; tests that depend on
  filesystem modes or a host `gzip`; a `#[cfg(test)]` path that differs from
  production; a test that passes only because `mock.py` is lenient.
- Harness coverage: `mock.py` fixtures (text, split, blank, slow, tool, fetch
  pages, paging, show, reason, empty, naming, compaction) and harness actions
  (`@resize`, `!keys`, `--work`, `--wait`, `--expect`/`--refute`,
  `--no-fetch-local`). Map each to the behavior it pins. Name the missing
  journeys: first-run chooser, resume replay, mid-turn queue and take-back,
  mode flip and restart, sandbox denies, MCP trust + second start, job
  cancellation, resize during a turn, piped output separation, terminal
  escape stripping.
- CI: `cargo fmt --check`, clippy, `cargo test --locked`, release build,
  the interactive harness job, and the 55% line-coverage gate. Check the
  coverage gate actually measures the changed code path (not just lines), the
  interactive job installs tmux, and a test that needs a host tool silently
  skips in CI. A gate that permits a whole module to rot is a finding.
- Regression discipline: sample recent commits (`git log`) and check a
  behavioral fix landed with a test. For each finding in this pipeline's other
  briefs that names a missing test, the right layer is: unit for pure logic,
  harness for the screen/sandbox/session lifecycle.
- Stability: a test that spawns a port or a process has a timeout and cleans
  up; a test that asserts timing is generous and not order-dependent; a test
  that reads a temp directory uses a unique one; a test that depends on the
  host's `gzip`/`tmux` reports a skip instead of passing silently.
- Gaps to name, not fix: the review may report a missing test without deciding
  whether the behavior is desired; the fix is "pin this invariant at this
  layer", not "add coverage for this function".

For each finding give the invariant or documented behavior, the nearest
existing test, `file:line` on both, and the failure a bug would produce that
the suite would miss. Close with the highest-value missing tests first
(likelihood × impact).

Out of scope: fixing the code under test, judging the implementation except
where it makes a test meaningless, and performance measurements (see
`performance-and-snappiness.md`).
