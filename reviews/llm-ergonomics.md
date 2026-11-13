# LLM ergonomics and documentation freshness

Axe review prompt — one concern: **the repository lets an agent find, trust and
change the right thing without holding the whole codebase in context, and
every doc claim matches the code it describes.**

Read first: `README.md`, `AGENTS.md`, every module's `//!` header, the skills
under `.agents/skills/`, and the harness's own `--help`. This review is about
the docs-to-code contract and the shape of the code for a reader, not runtime
behavior.

Verify with depth — sample every layer and every doc claim, do not skim:

- README as the user contract: every table (tools, config, state files, MCP,
  modes, keys, commands), every limit (50 000, 30 000, 48 KB, 1 MiB, 120 s,
  16 KB, 200 skills, 128 tools, 256 KiB, 4 MiB) and every path must match the
  code. Check the install/build/run instructions, the version sentence, and
  the `about` path: the binary returns README byte-for-byte, so a stale line is
  shipped in the product.
- `AGENTS.md`: every numbered invariant must be true today; the architecture
  list must name every module and no removed one (the recent `main.rs` split
  into `repl.rs`/`startup.rs` is a check); the conventions (errors, transcript
  lines, markdown, colors, commit style) must match the code; the provider
  notes and build/test commands must be current; the harness examples must
  actually run.
- Module headers: each `//!` names the responsibility, the invariants it owns,
  and its collaborators. Flag a header that restates the filename, a module
  with no header, and a claim the code no longer makes (for example the HTTP
  module's body decoders, the sandbox's allowlist, the UI's layout rule). Flag
  an invariant that lives only in a header where no test can pin it.
- Comments at the point of enforcement: lock ordering, append-only history,
  prefix stability, mode notes, truncation, sanitizer, trust hashes, and
  `unsafe` blocks. A comment must say why, not what; a comment that contradicts
  the code is a finding, and so is a subtle rule with no comment where it is
  enforced.
- Cross-references: README → AGENTS.md → module headers → tests. Every
  mirrored fact (a limit, a path, a mode rule) should have one authoritative
  home. List each duplicated behavioral fact and say which copy is canonical;
  a doc that points instead of duplicating is the preferred fix.
- One term per concept: session body versus sidecar, live history versus
  session file, turn versus round, frame versus prompt, block versus line,
  notice versus status, catalogue versus listing, scratch versus plan. Flag a
  concept with two names across layers and an identifier reused for two
  concepts, especially in public function names and tool descriptions.
- Navigability: entry points are obvious (`main`, `startup::run`,
  `Session`), no module is unmentioned in any map, no doc link or path is
  dead, and the commands a user needs are reachable from `/help` and `about`.
  Check a newcomer can answer "where does a key event go" and "where does a
  session write" from the docs alone.
- Size and cohesion from a reviewer's viewpoint: `ui.rs`, `main.rs`, `mcp.rs`
  and `markdown.rs` are the large ones. Report a file only when the mixing
  hides correctness or forces a reader to hold two invariants at once; propose
  a clean split boundary, not a line-count refactor.
- Repo hygiene for an agent: `TODO`/`HACK`/`FIXME`, a comment that names a
  removed function, a generated or vendored directory that `rg` will walk
  (`references/`, `target/`), and a test fixture the docs do not explain. Check
  that `references/` is ignored and not part of the build.
- Skills and instructions: `.agents/skills/*/SKILL.md` and the review pipeline
  must describe the commands that exist (the `ship` skill's gate, the
  `next-review` skill's queue). A skill that tells an agent to run a removed
  command is a finding.

For each finding give the doc line and the code line (`file:line` on both
sides), say which side is wrong, and the concrete cost: an agent edits the
wrong module, a user trusts a limit that is not enforced, a stale command fails,
or two docs disagree and neither can be trusted.

Out of scope: runtime correctness itself (each concern's own brief), README
claims about sandbox enforcement (see `sandbox.md`) and MCP trust (see
`config-and-mcp.md`; here only the doc/code mismatch), and test coverage (see
`test-coverage.md`).
