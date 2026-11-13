# Sandbox and confinement

Axe review prompt — one concern: **explore mode's guarantees are enforced by
the kernel, and every claim the mode note and README make about them is true on
each platform axe supports.**

Read first: `AGENTS.md` invariant 15 (and 8), README "Modes", "Sandbox",
"Trust and security"; then `src/sandbox.rs` in full, `src/tools.rs` (`execute`,
`bash`, the mode gating and denial text), `src/mcp.rs` (spawn and the mode-flip
restart), and the harness scenarios that assert denials.

Verify with depth — deny first, allow second:

- Ruleset construction: the handled-access mask, the read versus write-only
  rules, and the two writable directories (session scratch, project `plan/`)
  granted before restriction and nothing else. Check paths that do not exist,
  a symlink resolving out, a rename or unlink whose parent is unwritable, a
  hardlink, and a write through `O_TMPFILE` or an inherited descriptor.
- Exec: `COMMANDS` and resolution — PATH lookup, absolute and relative
  commands, a word outside the list, an allowlisted wrapper reaching an unlisted
  program (`env`, `xargs`, `find -exec`, `sh -c`), an interpreter running an
  unlisted program, a shebang, and exec via a stdio MCP server. State what
  containment actually stops an exec, since the leading-word check is not it.
  Check the ELF interpreter (`PT_INTERP`) and git's transport execs.
- Seccomp: `io_uring_setup` denied, the arch check, EPERM versus death, and the
  fallback when the filter cannot be installed. Check whether the filter
  remains installed across `execve` into an allowed program.
- ABI and degradation: Landlock ABI probing, older or absent Landlock, the one
  startup line, and whether the mode note still implies confinement it does
  not have. macOS Seatbelt: profile, denies, the same writable set, and the
  same exec behavior.
- Process state: `CHILD_ENV` and which variables survive, `no_new_privs` in
  both modes, cwd, inherited fds, signals and ptrace against the parent. Check
  a child that daemonizes, a setuid binary, and a child that tries to write
  after the parent closes the scratch directory.
- MCP: a stdio server gets the write rules and the `io_uring` rule in explore
  mode, keeps exec, and is restarted by a mode flip; an HTTP server has no
  process and no claim. Verify the write-only ruleset is applied to the spawned
  server, not just to `bash`, and that a restart cannot leave an unconfined
  process behind.
- Escape hatches: `!cmd` is the user's shell in both modes; `fetch` and
  `web_search` are refused outside explore; `AXE_FETCH_ALLOW_LOCAL` is ignored
  by a release build. Name every path writable or executable that the design
  says is not, and every claim in README "Sandbox" that is stronger than the
  kernel rule.
- Evidence: the deny scenarios (`echo x > /tmp/p`, an unlisted command), the
  allow scenarios (`head`, `git ls-remote`), and a `!cmd` escape reproduce. A
  test that would pass with the ruleset dropped is a finding; a comment that
  says a check is kernel-enforced where it is a string check is a finding too.

For each finding give the syscall or rule at `file:line`, the invariant
violated, and what a model can write or exec that the docs say it cannot.
Close with the verified-clean list and any claim that needs a live run (name
the exact `!` command).

Out of scope: network confinement (there is none in either mode; report only
claims), MCP protocol and config semantics (see `config-and-mcp.md`), tool
descriptions (see `tool-contract.md`), and terminal rendering of denial text
(see `rendering.md`).
