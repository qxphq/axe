# Deslopification

Axe review prompt — one concern: **every line earns its place; nothing exists
only because it was cheap to generate.**

Read first: `AGENTS.md` principles 1–3 (dependencies are debt, no speculative
features, small surface), 7 (an argument its schema typed is never silently
defaulted), 10 (a typo must never change what a session does in silence) and
11 (no speculative fixes; reproduce first), then its Conventions ("keep
functions short; comment only non-obvious behavior", errors are `String`).
Then scan all of `src/`, `tests/`, `harness.py`, `mock.py`, `install.sh`,
`flake.nix` and `.github/workflows/`.

Definition: slop is code, comment, test, or doc text whose deletion or
collapse changes nothing a user, a test, or an `AGENTS.md` invariant depends
on — or whose presence hides a failure the contract wants surfaced. The
removal test decides: delete or collapse it; if behavior is unchanged (or an
error now reaches the user or the model where the contract says it should), it
was slop.

Verify with depth — enumerate mechanically per tree (the pattern lists below),
then judge each hit, do not skim. Every tree gets every category; an earlier
review that touched the same code (a domain brief, the test-coverage pass)
does not count as coverage:

- No failure-hiding fallbacks: every swallow site from the tree's pattern list
  either has a reason the contract or a comment at the site gives (a write to
  a terminal that is gone, best-effort cleanup of a scratch file, a tool error
  that becomes a result for the model) or is a finding. A fallback that
  substitutes a plausible value for an error — a default for a malformed
  config value, an empty list for an unreadable session, `0` for an
  unparseable usage field — is the most harmful slop, because invariants 7
  and 10 exist to forbid exactly that.
- No improbable-case defense: guards, retries, re-validation, and recovery
  paths for states the code's own invariants make impossible (a value
  `config.rs` already validated, a race the one-writer screen or the single
  `mpsc` consumer rules out, an internal call that cannot fail). These should
  be a plain `expect` or nothing; a branch nobody can reach is slop even when
  it looks careful.
- Fixes stayed cheap: where a change added machinery (a cache, a flag, a
  `Screen` or `Session` field, a retry loop, a second code path) to handle a
  case that one early check, one guard clause, or skipping the whole path
  would handle, the simpler alternative is the finding. Compare the recent
  `Close … review findings` and other fix commits (`git log`) against the
  cheapest fix that meets the requirement.
- No dead or vestigial code: `pub` items with no caller outside their module
  or only in tests, parameters every caller passes the same value, branches,
  config keys or `Mode` cases no path reaches, half-removed features (a
  `Paint` variant nothing paints, a `mock.py` reply no harness journey asks
  for, a slash command `/help` no longer lists), `#[allow(dead_code)]`, and
  `#[cfg(test)]` helpers that exist only to be tested.
- No duplicated facts or logic: the same literal (a cap, a path segment, a
  timeout, an ANSI sequence outside `text.rs`), helper, or near-identical body
  in two or more modules, or in `harness.py` and `mock.py`; one owner,
  referenced everywhere. A style defined outside `Paint` is this category.
- No avoidable ceremony: pass-through functions that only rename, a trait or
  generic with one implementation, a struct or builder for one call site,
  hand-written plumbing an existing helper in the same module covers, and
  knobs "for future flexibility" — a config key, a CLI flag, an env variable —
  with no demonstrated need (principle 2). A dependency or crate feature the
  code no longer needs is this category too (principle 1).
- No comment slop: comments that restate the code, defensive essays
  justifying a guess, changelog-style "previously…"/"now…" notes, decorative
  banners, `//!` headers padded past responsibility, invariants and
  collaborators, and comments that contradict the code.
- No test slop: assertions that recompute the implementation instead of
  pinning behavior, tests that still pass when the feature breaks (a harness
  `--expect` that matches the echoed prompt rather than the result, a `sleep`
  standing in for a condition, a skip that reports a pass), and copy-pasted
  bodies that should be one table-driven test.
- No verbose or hedged code where it obscures logic: the same input checked
  twice on one path, needless `clone`/`as`/`Option`/`Result` wrapping, the
  same failure reported twice (a notice and an error line for one event),
  `if x { true } else { false }`. Report only where it hides what the code
  does, never as pure style or a lint clippy already owns.
- No unverified claims: numbers and certainties in comments, `//!` headers,
  tool descriptions and the system prompt (sizes, timeouts, "always",
  "never", "cannot") that nothing measures or checks; point to the check or
  flag the claim. Prompt and tool text is code the model reads every
  request: a sentence whose deletion changes no model behavior is slop, but
  its wording is changed deliberately (Conventions), so such a finding needs
  the user's input.

Enumeration patterns per tree (starting points, not limits; add the tree's
own idioms):

- `src/` (Rust): `let _ =`, `.ok()`/`.ok()?` discards, `unwrap_or`/
  `unwrap_or_default`/`unwrap_or_else(|_| …)`, `Err(_) =>`, `if let Ok` with
  no `else`, `PoisonError::into_inner` and other lock-poison handling,
  `unreachable!` after a manual dispatch, `#[allow(...)]`, `#[cfg(test)]`-only
  items, `.clone()` and `as` casts without need, `serde_json` lookups that
  default instead of erroring (`.as_str().unwrap_or("")`, `.as_u64()
  .unwrap_or(0)`), and `format!` error strings repeated with one word changed.
- `tests/` and `#[cfg(test)]` modules: constant-equals-constant assertions,
  recomputed formulas, sleeps standing in for a condition, tests that skip
  silently when a host tool (`gzip`, `tmux`, `sh`) is missing, copy-pasted
  bodies.
- `harness.py`, `mock.py`: bare or broad `except`, `except …: pass`,
  `time.sleep` where a screen condition could be polled, fixtures and replies
  nothing calls, option or path literals duplicated between the two.
- `install.sh`, `flake.nix`, `.github/workflows/`: `|| true`, `2>/dev/null`,
  `continue-on-error`, missing `set -eu`, `! cmd` under `set -e`, duplicated
  version, path or target literals, unchecked version or size claims.

Coverage table (required in the report, after the checks): one row per tree
× category with the sites enumerated and the findings, so a thin row is
visible before triage rather than discovered after it.

Per finding, beyond the common fields: the category, what deleting or
collapsing it changes ("nothing" or "surfaces a failure", and to whom: the
user, the model, a test), and the net lines it removes.

Fix rule: a deslop fix removes or collapses code. Collapsing repetition into
one small private helper inside the owning module is a collapse; a new
module, trait, or cross-module abstraction to replace slop is not
(principle 3). Removing a failure-hiding fallback gets a regression test
proving the error now surfaces — a unit test for pure logic, a `harness.py`
journey when only the terminal shows it. Removing a config key, flag or
documented behavior changes the README in the same commit and is a
needs-input finding, since a user may rely on it.

Out of scope: doc freshness and cross-references (see `llm-ergonomics.md`),
missing tests as such (see `test-coverage.md`), the token cost of prompt text
(see `token-efficiency.md`) and its wording (see `prompt-prefix.md`),
correctness bugs that are not slop (see the domain briefs), and formatting
or anything `cargo fmt` and clippy already enforce.

Order findings within a severity: failure-hiding first, then improbable-case
defense and over-complex fixes, then by net lines removed.
