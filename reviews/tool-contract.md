# Tool contract

Axe review prompt — one concern: **the seven tools' names, descriptions and
schemas promise exactly what `execute` does, and every refusal, cap and error
is a result the model can act on.**

Read first: `AGENTS.md` invariants 7–8 and 15, README "Tools"; `src/tools.rs`
in full (the `Spec` table, `execute`, `read`, `write`, `edit`, `bash`, the
truncation constants, `Mode`), `src/api.rs` (how the array and tool messages
are sent), `src/render.rs` (how a call is shown), `src/mcp.rs` (`definitions`,
`call`), and the `tool:` scenarios in `harness.py`.

Verify with depth — call each tool with the hard arguments:

- `read`: 1-based `offset`, `limit` as a line count, `offset` past the end,
  `limit` 0, a missing file, a directory, a binary file, a file without a final
  newline, CRLF, a single line over 50 000 chars, and 1 MiB on disk. Check the
  returned window is exactly what the description promises and that the cap
  cannot split a UTF-8 char.
- `write`: parent directories created, an existing file truncated, a symlink,
  a path outside cwd, a read-only directory, an empty content, a content with
  a NUL, and a path in explore mode (the tool is absent, but a crafted call
  must still be refused). Check the mode and ownership of the created file.
- `edit`: `old` empty, `old` matching zero/one/many times, `replace_all` true
  and false, overlapping matches, a match at the file boundary, CRLF versus LF,
  a match split by a UTF-8 boundary, and identical `old` and `new`. Check
  `old` ambiguity is an error, not a first-match replacement.
- `bash`: 120 s timeout, `sh -c` semantics, cwd, no state between calls, exit
  code, stdout/stderr ordering, a command that prints 64 MiB, a command that
  ignores SIGTERM, a cancelled command, and a denied command versus a missing
  binary. Check the 30 000-char model cap and the temp file holding the full
  output; check that the marker says how to read the rest.
- `about`: returns the compiled README byte-for-byte, works with no checkout,
  and is not silently truncated. Check that the README's claims about tools,
  limits and paths match the code (the contract itself lives in
  `llm-ergonomics.md`; here verify the tool's result).
- `fetch` and `web_search`: argument validation, `start`/`chars` bounds,
  `count` bounds, refusal outside explore mode, and the source label. Check a
  missing required argument, an unknown extra key, a wrong type (string for an
  integer), and invalid JSON.
- Mode gating: explore removes `write`/`edit` from the array and refuses them
  in `execute`; normal refuses the web tools. Check an MCP tool is not gated
  by accident, and that a mode flip does not leave a stale array.
- Errors as results: unknown tool, malformed arguments, missing argument,
  tool panic, a sandbox denial, a provider-independent failure. Check the
  wording distinguishes "you cannot do this here" from "this failed" and from
  "this does not exist", and that no error is a harness crash.
- Caps and truncation: exact boundary for 50 000 and 30 000, a multi-byte
  character at the cut, head-and-tail versus head-only, the marker text, and
  where the full output lives. Check the model can continue without re-running
  a destructive command.
- The call line: name, arguments collapsed to one line, a multi-line shell
  command, a very long argument, an error glyph, and consecutive calls as one
  block. Check the transcript never shows a result the model did not receive.
- `!cmd`: runs through the same `execute` path, joins as a user message, runs
  no turn, leaves the call-style echo with the command under it, and bypasses
  explore write confinement. Check its output cap and cancellation.
- Tests: for every cap, refusal, ambiguity and mode gate above, say whether a
  test pins it. A schema sentence with no test is a gap, not automatically a
  bug; name the invariant the test should pin and where it belongs.

For each finding give the call's JSON, `file:line`, the description or schema
sentence it contradicts, and the concrete outcome: the model wastes a round on
a wrong assumption, a cap silently loses the error, an edit changes the wrong
text, or a tool succeeds where the mode says it must not.

Out of scope: kernel confinement (see `sandbox.md`), HTTP/web parsing (see
`http-and-sse.md`, `web-tools.md`), MCP transport and config (see
`config-and-mcp.md`), and prompt wording beyond the tool contract (see
`prompt-prefix.md`).
