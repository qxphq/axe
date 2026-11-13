# Trust boundaries and secret hygiene

Axe review prompt — one concern: **every trust claim in the README matches what
the binary enforces, and no secret reaches disk, a child process, a log, or a
request it was not meant for.**

Read first: README "Trust and security" in full, "Modes", "Config", "MCP
servers"; `AGENTS.md` invariants 8–10 and 15. Then `src/config.rs` (key
resolution, literal-key rejection), `src/tools.rs` (`read`'s `/proc` refusal,
the child environment, `bash`), `src/state.rs` and `src/sessions.rs` (modes),
`src/prompt.rs` (project wrapper), `src/web.rs` (source labels and sanitizer),
`src/mcp.rs` (env/header expansion, trust prompt), `src/text.rs` (terminal
escapes), and `main.rs`/`startup.rs` (banner, info row, errors).

Verify with depth — follow the secret and the untrusted text:

- Secret inputs: `api_key_env` only; a literal `api_key` is rejected; a missing
  variable skips the provider instead of sending an empty bearer; Brave's token
  via `search.brave`; MCP `headers` and `env` values expanded from axe's
  environment. Check a token with quotes, a newline, or a non-ASCII byte; a
  variable that is set but empty; and a search key configured but invalid.
- Where a secret can surface: the request headers, an error body from the
  provider, a session JSONL line, the info row, the splash, a warning, an MCP
  error, the transcript of a failed fetch. Check `read` refusing `/proc`
  environment files (including `/proc/self/environ`, `/proc/<pid>/environ`,
  symlinks, `/dev/fd`), a process's `environ` through `bash`, and core dumps or
  temp files. Check that a provider error is not printed with its
  `Authorization` header.
- Child environments: command children get only `PATH`, `HOME`, `LANG`,
  `LC_ALL`, `TERM`, `TMPDIR`, `USER`, `SHELL`; MCP servers get those plus the
  variables their entry names. Check a provider key that happens to be named in
  `CHILD_ENV`, a variable inherited through a shell function or `LD_PRELOAD`,
  and whether `env` inside a command can see a token.
- File permissions: directories `0700`, files `0600` for state, sessions,
  history, trust, plan; temp files for bash output and MCP results; an existing
  file with looser modes. Check the config and MCP files' modes after `/mcp
  add`, and whether a failed write leaves a partially written secret-bearing
  file.
- Trust of repository text: a project `AGENTS.md` and its skills load without a
  prompt by decision; the prompt wraps `AGENTS.md` and the first rule calls all
  such content data. Check the wrapper's path escaping, a file containing the
  closing tag, a skill body that instructs the model, and the catalogue's
  description of a skill. Check that a fetched page, search snippet and MCP
  result are labelled untrusted and that the label cannot be spoofed by the
  content's first line.
- `.mcp.json`: the one content that needs approval per path and content hash.
  Check what the hash covers (whitespace, comments, key order, an edit, a
  deletion), what the question shows (every command and every variable name,
  not values), a non-interactive or piped start, a project file above cwd, and
  a project entry that only toggles a global one. Check the trust record's
  permissions and whether `/mcp forget` clears it.
- Modes: explore removes `write`/`edit` from the array and refuses them if
  called anyway; normal refuses `fetch`/`web_search`; `/new` resets explore on;
  the mode note says what is true. Check that the note, the tool array and
  `execute` agree after a flip, a resume, and a model switch.
- Terminal safety: source labels, project paths, model names, tool arguments,
  fetched pages and command output must not carry ESC/CSI/OSC/C1 into the
  terminal; clipboard and title sequences must be stripped, not just colored
  over. Check `text::strip_control`, `terminal_text`, and every path that
  prints content without going through them.
- Debug and test knobs: `AXE_FETCH_ALLOW_LOCAL` must be compiled out of a
  release (verify the `cfg`, not the comment), the harness's local fixture
  server must not be reachable from a release, and a test-only environment
  variable must not become a production switch.
- The claims themselves: "runs as you", "normal mode runs code as you", "the
  network is not confined", "reads are not confined", "prompt injection cannot
  be sanitized away", "the provider is inside the trust boundary". Find any
  claim that is weaker or stronger than the code, and say which side to change.

For each finding give the input or configuration, `file:line`, the boundary
crossed, and the concrete outcome: a token visible in a file or child, an
untrusted string treated as authority, a write that escapes explore mode by a
documented-but-wrong path, or a README sentence that would mislead a user into
trusting it.

Out of scope: kernel confinement mechanics (see `sandbox.md`), HTML/parser
correctness (see `web-tools.md`), and MCP config validation and transport
details (see `config-and-mcp.md`).
