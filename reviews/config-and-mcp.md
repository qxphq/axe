# Config and MCP

Axe review prompt — one concern: **both input files are parsed strictly and
never silently guessed, and an MCP server runs only what the files authorize,
over the transport the entry named.**

Read first: `AGENTS.md` invariants 4, 9–10 and the MCP bullet in Conventions;
README "Config" and "MCP servers" in full, plus "Trust and security" for the
`.mcp.json` approval. Then `src/config.rs` in full, `src/mcp.rs` (paths, parse,
merge, trust, both transports, `definitions`, `call`, `restart`, `/mcp`
add/remove/forget), `src/startup.rs` (config load and server start), and
`src/main.rs` (`/mcp`, `/explore` restart).

Verify with depth — parse bad input as carefully as good:

- Config path and lifecycle: `$XDG_CONFIG_HOME/axe/config.json`, the default,
  auto-create only when missing, never rewritten afterwards, a directory that
  cannot be created, a file with a BOM or trailing comma, and a JSON error that
  names the file. Check the example text is valid on first run and that the
  config is not read twice.
- Provider entries: `providers` map or top-level `base_url` for `local`, a
  duplicate name, a missing/relative/invalid `base_url`, `models_url`,
  `api_key_env`, `session_header`, `thinking` levels and the `{}` default, and
  an `api_key` literal that must be rejected. Check an unknown provider key,
  a missing model, a stale model id, and a disabled or unset provider.
- Global keys: `model` spec parsing, `explore_by_default` (default true),
  `system`, `agents` (`~/` and config-relative), `skills` (directory or
  `SKILL.md`), `thinking` map, `search.brave`. Check a wrong type for each,
  a missing file, a 48 KB `AGENTS.md`, and an unknown top-level key.
- MCP files: global `$XDG_CONFIG_HOME/axe/mcp.json`, project `.mcp.json` at or
  above cwd and never past the repository root. Check a project file in a
  directory without `.git`, a nested repo, a symlinked cwd, two project files
  (nearest wins), and a malformed file that must not prevent the other one
  from loading. Check the `mcpServers` shape, `command` xor `url`, `args`,
  `env` (map or array shorthand), `headers`, `enabled`, `timeout`, `type`, and
  an unknown key. Check `~` in `args`, `$NAME`/`${NAME}`/`$$` expansion, a
  missing variable (reported by name, server not half-started), and a value
  that expands to a newline.
- Merge semantics: a project entry replaces a global one of the same name; an
  entry with no `command`/`url` may only toggle `enabled`; a project cannot
  define a new server with a matching name but no command; a disabled global
  server stays disabled unless the project enables it. Check duplicate names
  within one file, ordering, and a project entry that tries to shadow a
  built-in tool name.
- Trust: the hash covers path and content; one approval per content hash; a
  whitespace or key-order edit re-asks; removal and `/mcp forget`; the record
  lives in `mcp-trust.json` under the project state. Check the question lists
  every command and every variable name (not values), a non-interactive/piped
  start (what happens when it cannot ask), a project file with a server that
  fails to start, and a forged trust record or symlink. Check modes `0600`.
- stdio transport: spawn with the child env, initialize handshake, protocol
  version, capabilities, `tools/list` (pagination/cursor), tool call ids,
  timeout, cancellation (server kept), stderr tail, a non-JSON line on stdout,
  server exit, a result over 4 MiB, and text extraction from content blocks
  (image/resource omitted). Check the reader thread cannot block the caller.
- HTTP transport: POST JSON-RPC, `Accept`, `Mcp-Session-Id` echo and DELETE,
  `MCP-Protocol-Version`, auth only from the entry's `headers`, a JSON body vs
  an SSE reply, a 401/403, session expiry, a redirect, a timeout, and two calls
  in flight. Check axe sends no token to a server the entry did not name and
  reads no other auth store.
- Names and caps: `mcp__<server>__<tool>`, the sanitize rule, the 64-character
  hash suffix, collisions with built-ins and between servers, 128 tools and
  256 KiB schema per server, and the 4 MiB message / 30 000-char result caps.
  Check a server name with `__`, punctuation, unicode, or an empty tool name.
- `/mcp`: `add [--project] [--url URL] [--env NAME[=VALUE]]... [--header 'K:
  V']... [--timeout S]`, `remove`, `forget`, `listing`, `detail`. Check the
  written JSON is valid and atomic, the file mode, an existing name, a flag
  with a missing value, a command with spaces, and a header value containing a
  colon. Check `/mcp` reports a file that changed since start and that a
  changed catalogue needs a restart.
- Mode flip: `/explore` restarts every server, the names stay the same, one
  that fails answers with an error, and the tool array is not rebuilt. Check a
  call in flight during the restart, a server that was disabled, and a remote
  server with no process.
- Startup and piped runs: no terminal means no trust question, no splash
  chooser, and a deterministic list of which servers start. Check a failing
  server does not abort the session and that its error is visible once.

For each finding give the file bytes or CLI arguments, `file:line`, the
invariant or README sentence it violates, and the concrete outcome: a server
starts that the file did not authorize, a token goes to the wrong host, a bad
config is silently defaulted, or an entry that should work cannot start.

Out of scope: kernel confinement of a server (see `sandbox.md`), MCP tool
behavior beyond the array and call path (see `tool-contract.md`), and prompt
cache effects of the catalogue (see `token-efficiency.md`).
