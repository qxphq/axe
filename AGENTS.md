# axe

Minimal coding agent harness in Rust: one binary, a line REPL, OpenAI-compatible
providers, seven tools. The code is the source of truth; this file records the
invariants a change must not break.

## Principles

1. **Dependencies are debt.** Prefer `std`; a crate must be strictly necessary and
   widely maintained. Current: `serde_json`, `rustls`, `webpki-roots`, `ratatui`
   (crossterm backend), `unicode-width`, and `libc` on Linux. Crypto and
   terminal geometry are not hand-rolled; compression is the host's `gzip`,
   never a crate.
2. **No speculative features.** Add a thing when its absence hurts. No alt-screen,
   panes, subagents, or permission layer until the REPL is genuinely
   insufficient. MCP servers are the one addition on that list: they are the
   only way to give the model a capability axe will never build in, and they
   are additive — no built-in tool moves behind one. One feature per change.
3. **Small surface.** Twenty-two modules; a new module needs a separate concern,
   not a line count.
4. **Provider-agnostic through the OpenAI-compatible API.** `chat/completions` +
   SSE only. Express differences as config (`base_url`, `models_url`,
   `api_key_env`, `session_header`), never per-provider code paths.
5. **The prompt is structure.** The built-in system prompt is cwd and a short
   numbered list of rules that hold for every user; tool names, descriptions and
   schemas travel only in the `tools` array. Rules are policy: one line each, no
   workflow, nothing project- or provider-specific. Instructions are appended in
   order: `agents` (config), the nearest `AGENTS.md` at or above cwd, then
   `system`, then the skills catalogue. The project file is wrapped in
   `<project_instructions path="...">` — and cannot close it — and the first
   rule calls it data, not authority. Read once at startup; never re-read mid-session.
6. **Stable prefix, append-only history.** System prompt and tool schemas first,
   volatile content last. Never inject timestamps, status, or cwd changes into the
   prefix: it invalidates provider prompt caching. Sessions are append-only JSONL,
   replayed on resume; `/compact` appends a marked summary and the loader starts
   from the last marker, so the bytes stay and the request shrinks.
7. **Tools are dumb.** `read`, `write`, `edit`, `bash`, `about`, `fetch`,
   `web_search`, plus whatever an MCP server contributes as
   `mcp__<server>__<tool>`. No `glob`, `grep`, or `list`; `bash` + `rg`/`fd`
   covers them. The built-in seven never become a server: their confinement,
   caps and schemas are axe's.
   `about` returns the compiled-in `README.md`: documentation, not a code path.
   `fetch`/`web_search` are HTTP(S) GET with the body reduced to text and
   labelled untrusted; both are refused outside explore mode. `curl` is on
   explore's allowlist because a command sends what `fetch` cannot. Tool errors
   are results for the model, not harness failures. Truncate output, timeout
   `bash`, cap `fetch` at 1 MiB/50 000 chars and `web_search` at 1 MiB/20
   results.
8. **No approval prompts.** Bash runs unprompted by explicit user decision. Do not
   add a permission layer without being asked. A directory's `AGENTS.md` and its
   skills also load without a question: a trust prompt gates text the model could
   read anyway, so it buys nothing but friction. The one exception is a
   project's `.mcp.json`: it names a program, not text, so the start asks once
   per content hash, and the question lists every command and variable name.
9. **Real cwd, relative paths.** `current_dir()` at startup, used for the prompt and
   every relative path. Never hardcode directories.
10. **One config file, and it is input.** `~/.config/axe/config.json` (respects
    `XDG_CONFIG_HOME`), auto-created on first run. Secrets only through
    `api_key_env`; never commit keys or read another app's auth store. Axe never
    writes it: the per-directory state and sessions live under
    `~/.local/state/axe/projects/<escaped cwd>/` (respects `XDG_STATE_HOME`).
    MCP servers are a second input, `mcp.json` beside it and `.mcp.json` in a
    project, in the ecosystem's `mcpServers` shape: a `command` (stdio) or a
    `url` (streamable HTTP), and values that may be `$NAME`/`${NAME}`
    references to axe's environment, expanded when the server starts. The
    binary writes those files only through an explicit
    `/mcp add`/`remove`/`forget`.
    Config `model` is the seed: the selection a directory starts on is its own
    `state.json`, then `<state tree>/default.json` (what `/model -g` writes),
    then the config.
11. **Verify against reality.** Test against `mock.py` (chunked SSE, keep-alive,
    session headers). No speculative fixes; reproduce first.
12. **A scrolling REPL, not a TUI.** No alt-screen: the transcript is
    scrollback. The only owned region is an inline live frame at the bottom
    (a blank row, the candidate list or a chooser's header and entries, a blank
    row, the queued lines and the row that hints at `↑`, a blank row, the
    wrapped prompt, a blank row, status, a blank row), and it is as tall as
    what it shows and no taller — `ui::Frame::layout` takes the rows the prompt
    wrapped to (as many as the terminal can spare, and no more), the queued
    lines, and the candidates. Its first row is blank whatever it draws, so
    nothing in the frame touches the transcript; the blank row above an idle
    prompt is that same row, not a special case.
    Nothing is written against the bottom edge, and no part of it shares a row
    with another: a frame too tall for the terminal gives up the list and the
    queue first, then crops the prompt's box, and drops the status only when
    the blank first row, a prompt row and the status cannot all stand. A chooser is the one part the
    terminal sizes: its header and entries take the rows, and when it closes
    without resuming its rows are committed to
    scrollback — except startup's chooser, whose list vanishes on the first
    typed key: typing is the choice of a new session. A terminal appends rows
    at the bottom and cannot take them back, so a frame that shrinks leaves
    blank rows above it; `slack` counts them and the next block fills them
    from the top. Everything above is appended, never repainted — except
    `/new`, which wipes the screen and its scrollback and starts the next
    session read-only: a session boundary.
13. **One writer, and it paints what is dirty.** While `ui::Screen` owns the
    terminal, only it draws. Every setter and transcript writer marks the screen
    dirty; `Screen::repaint()` paints only then, and the loops call it —
    `readline` on entry, on every key and on its idle tick, the turn and shell
    loops once per drained batch; a resize invalidates first, so the area under
    us is repainted even when no state changed. It also settles the frame back
    on the bottom — a reflow moves the text, and the inline area comes from the
    cursor it moved — and drops `slack`, because no row above the frame is
    known blank any more. No caller draws for a state change, so work that
    finishes off the UI thread cannot strand the row.
    Startup banner and piped output run before/without a `Screen`; a stray
    `print!` while it exists is a rendering bug. A piped run has no prompt,
    splash or notice — stdout is the model's text (and what a line asked for),
    and the harness's own lines, tool calls included, go to stderr. Only `Screen::write` and
    `Screen::settle` write rows above the frame, and a block goes where the
    transcript *ends* — `slack` counts the blank rows a frame that changed left
    above it, and `write` fills them from the top — so a block is always under
    the block before it, never under a gap.
14. **The prompt is a box.** The line being typed is bordered rather than
    glyph-prefixed: an outline one cell in from each side, a row of it above the
    text and a row below, one cell of padding between the text and the line, so
    a wrapped line reads as one block and the text stays on the theme's
    background. The outline is the prompt's cyan, and yellow while explore mode
    takes the writing tools away. A committed line carries `█ ` in the transcript,
    which has no frame. Named ANSI only, and both ends of the pair are the
    theme's.
15. **`bash` is confined by the kernel, not by strings.** Explore mode denies
    writes outside two directories — the session scratch, and the project's
    `plan/` under the state tree, which survives exit so a plan outlives the
    session; neither is executable — and exec outside a fixed allowlist
    (Landlock/Seatbelt, plus seccomp denying `io_uring_setup`, which Landlock
    cannot mediate); it degrades with one startup line, which the history
    carries too, because the model must not be told a rule the kernel is not
    holding. The allowlist is a
    convenience guard, not the containment: it carries the common inspection and
    tooling set, interpreters and package managers included, and leaves out
    daemons that write through another process (`docker`, `systemctl`), setuid,
    and mode/timestamp tools (`chmod`, `touch`), which Landlock cannot see.
    Normal mode keeps only `no_new_privs` and the scrubbed environment. The
    network is confined in neither mode: a child connects where the user
    connects, `!cmd` included. A leading-word check is never a substitute.
    A stdio MCP server gets the same two writable directories in explore mode
    and the same `io_uring` rule, and keeps exec: the allowlist is a
    convenience for `bash`, and `npx`/`uvx` start a runtime a leading-word
    check would refuse. An HTTP server has no process to confine, and the
    network is unconfined in both modes anyway. A process cannot be narrowed,
    so `/explore` restarts every server.
16. **One version, in `Cargo.toml`.** `env!("CARGO_PKG_VERSION")` is the banner
    and `--version`; `flake.nix` reads the same file. The `ship` skill bumps it
    per semver (pre-1.0: patch for a fix, minor for a capability, a breaking
    change bumps minor until `1.0.0`). No second copy, no generated file, no
    version tag: the release is a single mutable `latest` tag whose assets are
    clobbered on each push to `main`, and `install.sh` downloads from it
    (on NixOS it installs the flake instead).

## Architecture

- `main.rs` — what a session is: the lifecycle (startup chooser, `/sessions`,
  `/new`, naming), model and thinking selection (a chooser that typing narrows;
  no argument completion, because Enter would take the candidate), slash
  commands, the agent loop, and the two sinks. Two producers (keys, a turn) feed
  one `mpsc` channel; the main thread applies events and draws once per batch.
- `repl.rs` — the terminal's line loop: the key thread and the cursor-report
  filter, `readline`, the chooser's keys, `pump` for work behind a live prompt,
  and the queue the frame draws while a turn runs.
- `startup.rs` — the process bootstrap: config, providers, instructions,
  skills, the MCP start, the splash, and the session the first line runs. The
  model-selection strings the info row and the choosers report are here too,
  because this is where the choice is made.
- `prompt.rs` — the system prompt: the built-in rules, then the instructions and
  the skills catalogue, built once as the prefix every request shares.
- `store.rs` — the session on disk and the live history that feeds it: one
  `push` appends to both, and the bytes the info row's fill is estimated from
  are counted there.
- `ui.rs` — the screen: the live frame (`Frame`, a layout from what the frame
  shows — the prompt's rows, the queued lines, the candidates — and from the
  mode
  for a chooser, put in place by `settle`), key mapping, completion, the chooser,
  `write` for transcript blocks (wrapped here, since a clipped line would be
  lost) and the `slack` it places them by. The info row is pieces (`Info`), each
  with its own ANSI color: the caller says what a piece is, the screen how it
  looks. `Block` and `separated` are the one rule for the blank row between two
  blocks, which both writers use.
- `editor.rs` — the prompt's buffer and motions: a char cursor, the kill ring's
  one slot, and the keys readline taught everyone. No screen, no menu, no turn.
- `complete.rs` — path completion: the token at the cursor, listed against the
  working directory as whole lines. Directories carry a `/`, dotfiles wait to be
  asked for, and `@` is a trigger the completion consumes. Only a line's first
  token completes without it: a path in a message is prose. `ui` reaches it
  through a function pointer, so the screen never touches the filesystem.
- `render.rs` — transcript text: `Paint`, and what a call, a command and a
  reasoning line look like. The screen renders the same spans as cells.
- `turn.rs` — the turn loop: one request per round, the queue a typed line joins
  at a boundary, the two sinks, and `History` (the one place a message is both
  appended to the request and written to the log).
- `text.rs` — control bytes, the output cap, and the one place a `Style` becomes
  ANSI.
- `markdown.rs` — markdown: blocks, inlines, tables, entities, URL checks, and the
  renderer. `Stream` feeds it a line at a time and lays a block out when it ends.
- `api.rs` — model listing, chat request, SSE parsing, tool-call accumulation.
- `http.rs` — URL parsing, HTTP/1.1 over `TcpStream`/rustls with request bodies
  and response headers, body decoding (`Content-Length`, chunked, EOF), line,
  header and body caps, timeouts, cancellation by closing the socket.
- `config.rs` — load and validate the config; resolve the `agents` file.
- `state.rs` — per-directory state: the cwd key, load, an atomic write, the
  prompt history file `Up` and Ctrl+R walk, and `default.json`, the model a
  directory without a selection starts on.
- `sessions.rs` — meta sidecar and append-only JSONL body per session; listing,
  search, system gzip for old bodies, UTC stamp, and the compaction marker a
  body continues from.
- `tools.rs` — tool registry, schemas, generated prompt, local tools, `about`,
  and `Mode`: the tool set a session runs with, and the note and denial that
  announce it.
- `web.rs` — `fetch`/`web_search`: host guard, HTML tokenizer, sanitizer,
  Brave/DuckDuckGo parsers, paging.
- `skills.rs` — Agent Skills discovery, the frontmatter subset, the catalogue,
  and the body read on `/skill:` activation.
- `sandbox.rs` — the only `unsafe`: seccomp, Landlock, Seatbelt, exec allowlist,
  and the two directories explore mode may write: the session's scratch and the
  project's plan. The write-only ruleset an MCP server gets is built here too.
- `mcp.rs` — MCP servers: the two config files and their merge, the project
  file's approval record, both transports (a process with its reader thread, or
  streamable HTTP with session ids and SSE), the headers a remote request
  carries, tool names, the fixed catalogue, `/mcp`, and the restart a mode flip
  needs.

## Conventions

- Errors are `String`, user-facing, prefixed with context (`connect host:port: ...`).
  No error crates.
- Keep functions short; comment only non-obvious behavior.
- Transcript output is complete lines. `Screen::text` collects markdown;
  `Screen::output` writes the binary's own text one line per line. A block never
  begins or ends with a blank row: the one blank between two blocks is the
  writer's, and consecutive tool calls are one block.
- Markdown is the model's and the user's, never the binary's. Blocks are laid out
  when they end; the live row says what the turn is doing, not the unfinished line.
  A stream lays out the blocks the same text at once would: a blank line ends a
  block only when the next line cannot continue it, and for a list that is an
  item at an indent already open. A row wider than the field takes the rows it
  needs — a code row keeps its indent, and the quote or item it sits in, on
  every row it takes.
- Style comes from `Paint` in `render.rs`, rendered as cells or ANSI: it is the
  only table of what a thing looks like, and `Info` names a variant rather than
  defining one.
- Colors are the terminal's, never axe's: named ANSI only (`Color::Cyan`,
  `Color::Red`, …), never `Rgb`, `Indexed`, or a hex value of our own. The
  user's theme decides what each one is, so axe needs no theme and lands in the
  shell's colors wherever it runs. A new color is a new `Paint` variant, and it
  has to mean something.
- Tool names and descriptions are part of the prompt. Change them deliberately.
- `README.md` is the user-facing contract and the GitHub page. It ships through
  `about`, so keep it factual and in the same commit as the behavior it describes.
- MCP: a server's tools are read once, at startup, because tool schemas are the
  cached prefix — a restart is the boundary, and `/mcp` says when a file
  changed. Names are `mcp__<server>__<tool>`, sanitized and hash-suffixed past
  64 characters, so a server can never shadow a built-in or another server's
  tool. A project entry may replace a global one by name, but an entry with no
  `command`/`url` may only switch `enabled`: a repository narrows what runs,
  never defines it. The file is code, so it needs one approval per content hash
  (`mcp-trust.json` in the project's state directory), and the prompt shows the
  commands and the `$NAME` variables each entry reads. A remote entry
  authenticates with the bearer its own `headers` name; axe never sends a
  token to a server the entry did not name, and reads no other auth store.
- Skills: a directory with `SKILL.md` is the unit; only the frontmatter is read at
  startup, the body on demand. The catalogue is appended last in the system
  prompt, after the instructions, so the part above it stays byte-identical.
  Project roots stop at the repository root; without a `.git`, only cwd counts.
- A line typed during a turn joins it at the next round boundary: the turn's
  thread drains the queue there and appends every line to the history, so the
  model reads it while it works instead of after the last round. A round already
  streaming cannot see it, and a command — `/model`, `/sessions`, `!cmd` — needs
  the session and the screen, so it waits for the turn to end and is dispatched
  by the REPL loop like any line typed while idle. The frame stacks the waiting
  lines, one row each, under a blank row and over the hint; `↑` takes every line
  the turn has not read back onto the prompt, above what was typed, and empties
  the queue. A queued line is taken, not committed, so its memory is owed where
  it lands — the turn carries the lines it read out of the drain, the loop
  remembers the ones handed back — and every submitted line is remembered once.
- Sessions: `<id>.json` is the mutable sidecar; `<id>.jsonl` is append-only; bodies
  older than `sessions::KEEP` are gzipped. The sidecar stays readable, so listing
  and search work without unpacking. A compacted body carries a marked summary
  and the loader returns the messages from the last marker on, with the marker's
  key stripped: the marker is a file record, never a request field. The summary
  reaches the transcript once, as its own block under the notice that records
  it; a resume starts from it without printing it again. The system prompt and
  mode notes are never logged; resume rebuilds them. A title and summary are
  generated once, after the first answer; failing that, the first request
  stands in. The project directory also holds `plan/`, the one writable path
  explore mode keeps across sessions, `history.jsonl`, the prompt lines typed
  there: one JSON string per line,
  the newest 500, the same `0600`, and `mcp-trust.json`, the approved project
  MCP files by path and content hash. A blank, a leading space and a repeat of
  the newest are not steps.
- The model reads the line as it was committed: `parse_command` matches the
  trimmed form, so a command is typed with spaces around it, but the message it
  hands over is the bytes the transcript echoed. A line of nothing but spaces is
  no message at all — it leaves no block and no history step.
- `!cmd` is the user's shell. It runs through `tools::execute`, its output joins
  the history as a `user` message, no turn follows, and explore-mode write
  confinement does not apply. It leaves the line a call leaves — `render`'s
  `shell_echo`, glyph and all — and its words under it, not the `exit: N` header.
- Commit messages: imperative, one feature per commit.

## Provider notes

- `llm.local`: `http://llm.local:8000/v1`, plaintext, close-delimited bodies.
- DeepSeek: `https://api.deepseek.com`, `DEEPSEEK_API_KEY`; model list reports
  `context_window`.
- OpenCode Go: chat `https://opencode.ai/inference/go/openai/v1`, models
  `https://opencode.ai/inference/go/v1/models`, `OPENCODE_API_KEY`, stable
  `x-opencode-session`. Free-tier models 403 outside OpenCode. Console keys expire;
  an OAuth access token also works as a Bearer token.

## Build and test

```sh
cargo build
cargo test     # pure logic: markdown, parsers, caps, command parsing, keys
cargo fmt --check
cargo clippy --all-targets -- -D warnings
```

`harness.py` is the UI loop: `mock.py` + tmux pty + virtual terminal. It needs
python3 and tmux. `--work DIR` keeps one directory and its state; `--wait` and
`--timeout` bound idling; `--bin` selects the binary; `--ansi`/`--scroll` control
the dump, and `--expect`/`--refute` make it an assertion. See `harness.py --help`
and `mock.py` for fixtures and replies.

`@resize:COLSxROWS` resizes the pane between steps, which is what a terminal
split does; it is how the frame's re-anchoring is checked.

`fetch:PAGE` has the mock call `fetch` on a fixture page (plain, inject, big,
image, redirect, latin1, slow); `PAGE@START+CHARS` sets paging. The harness serves
those pages on loopback and sets `AXE_FETCH_ALLOW_LOCAL=1`; that variable is a
**debug- or test-build knob only, off by default; a release build ignores it**.
`--no-fetch-local` checks the refusal. `show:x` streams the last tool result back.

`web_search` asks fixed public hosts; `tool:` can call it. A `tool:` turn keeps
calling the same tool, so end it with `!Escape` and read the result with `show:`;
the first line says which engine answered. `BRAVE_SEARCH_API_KEY` selects Brave;
unset means DuckDuckGo, and a bogus value exercises the fallback.

`/compact` needs a history to summarize: one turn, then the command. The mock
answers a request whose system prompt starts with "You summarize" with a fixed
markdown summary, which the transcript carries under the notice.

```sh
./harness.py --wait 1 'text:hi' '/compact' --expect 'Mock summary of the session'
```

Skills are checked with a `.agents/skills/<name>/SKILL.md` under `--work DIR`:
`/skills` lists it and `/skill:name` runs the body. The sandbox is checked the
same way: `tool:bash:{...}` takes JSON arguments and loops; end with `!Escape`
and `show:`. Normal mode is a `!cmd` line, which does not loop. Examples:

```sh
./harness.py --scroll 400 'tool:bash:{"command":"head -n 1 tmux.conf"}' '!Escape' 'show:'
./harness.py --scroll 400 'tool:bash:{"command":"echo x > /tmp/p"}' '!Escape' 'show:'
./harness.py --scroll 400 'tool:bash:{"command":"tmux -V"}' '!Escape' 'show:'
```

The first is the allow — `head` is on the list, and its work is still write-
confined. The other two deny on the kernel: an unwritable path, and an exec
outside the allowlist — interpreters are on that list deliberately (their
mutation is a write), so the deny needs a name the list does not carry. A
`!cmd` line is the escape hatch, and the harness reads an argument starting
with `!` as tmux keys, so prefix it with a space: `' !make'`.

MCP is checked with a project file that names `sh` and a fixture server: the
first start asks, `y` runs it, `!Escape`/`show:` after a `tool:mcp__t__echo:{}`
turn reads the answer, and a second start asks nothing because the hash matches.
A tool you have not configured is absent from the array, not refused.

```sh
mkdir -p w/.git && printf '{"mcpServers":{"t":{"command":"/bin/sh","args":["/abs/srv.sh"]}}}\n' > w/.mcp.json
./harness.py --work w --scroll 300 --timeout 12 'y' '/mcp'
```

The mid-turn queue is checked by queueing a line while a slow call runs, and
letting the mock answer it: the second argument lands during the tool call, and
the reply ends the turn instead of the `tool:` loop it would otherwise be
(`!Space` arguments hold the harness open until the boundary passes). `!Up`
during the same call takes the lines back instead: they land on the prompt, and
the turn ends without reading them.

```sh
./harness.py --wait 1 --scroll 20 'tool:bash:{"command":"python3 -c \"import time;time.sleep(6)\""}' hello '!Up' '!Escape' '!Escape' --expect '│ hello'
```

```sh
./harness.py --wait 1 'tool:bash:{"command":"python3 -c \"import time;time.sleep(6)\""}' 'text:done' '!Space' '!Space' '!Space' '!Space' '!Space' '!Space' '!Space' '!Space' '!Space' --expect done
```
