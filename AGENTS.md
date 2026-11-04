# nano

Minimal coding agent harness in Rust. One binary, a line REPL, OpenAI-compatible
providers, six tools. Roughly 8.5k LOC across ten files.

## Principles

1. **Dependencies are debt.** Prefer `std`. A crate must be strictly necessary and
   widely maintained. Current: `serde_json`, `rustls`, `webpki-roots`, `ratatui`
   (with its `crossterm` backend), `unicode-width`, `flate2` (pure-Rust
   `rust_backend`). `rustls` because crypto is never hand-rolled; `ratatui` because
   terminal geometry is the same class of problem — the hand-rolled strip renderer
   produced a new whitespace bug per fix; `flate2` because old session bodies are
   gzipped and a compression format is never hand-rolled either.
2. **No speculative features.** Add a thing when its absence hurts. No alt-screen,
   panes, compaction, subagents, MCP, or permission layer until the REPL is
   genuinely insufficient. One feature per change.
3. **Small surface.** Ten modules. A new module needs a reason.
4. **Provider-agnostic through the OpenAI-compatible API.** `chat/completions` + SSE
   only. Never add per-provider code paths; express differences as config
   (`base_url`, `models_url`, `api_key_env`, `session_header`).
5. **The prompt is structure.** The built-in system prompt is the working
   directory, a short numbered list of behavioral rules that hold for every user,
   and the available tools — rules and tools generated in `tools::system_prompt`,
   tools from the same registry used for the `tools` array. Rules are policy, not
   advice: one line each, no workflow, nothing project- or provider-specific.
   `system` from config is appended after it, never replaces it. The same rule
   covers instructions: `agents` (config) then the nearest `AGENTS.md` at or above
   cwd, each capped at 48 KB, then `system`. All read once at startup; never
   re-read mid-session.
6. **Stable prefix, append-only history.** System prompt and tool schemas first,
   volatile content last. Never inject timestamps, status, or cwd changes into the
   prefix: it invalidates provider prompt caching. Sessions are append-only JSONL,
   replayed on resume; compaction stays last.
7. **Tools are dumb.** `read`, `write`, `edit`, `bash`, `fetch`, `web_search`. No
   `glob`, `grep`, or `list`; `bash` + `rg`/`fd` covers them. `fetch` and
   `web_search` because explore mode's shell allowlist has no `curl`, so that mode
   otherwise cannot read the web; both are refused outside explore mode. Search
   uses Brave when a token is configured and DuckDuckGo's HTML endpoint otherwise;
   a Brave query that fails or returns nothing falls back. Tool errors are results
   for the model, not harness failures. Truncate output (30–50 KB), timeout `bash`
   (120 s), cap `fetch` at 1 MiB off the wire and 50 000 characters to the model,
   `web_search` at 1 MiB and 20 results.
8. **No approval prompts.** Bash runs unprompted by explicit user decision. Do not
   add a permission layer without being asked.
9. **Real cwd, relative paths.** `current_dir()` at startup, used for the prompt and
   every relative path. Never hardcode directories.
10. **One config file, and it is input.** `~/.config/nano/config.json` (respects
    `XDG_CONFIG_HOME`), auto-created with an example on first run. Secrets only
    through `api_key_env`. Never commit keys, never read another app's auth store.
    Nano never writes it: what the user picked is per-directory state, in
    `~/.local/state/nano/projects/<escaped cwd>/state.json` (respects
    `XDG_STATE_HOME`), alongside that directory's sessions. Its `model` is the
    default for a directory that has no state, so the config stays the answer to
    "when I do not say otherwise".
11. **Verify against reality.** Test locally against `mock.py` (chunked SSE,
    keep-alive comments, session headers). No speculative fixes; only fix what a
    reproduction shows.
12. **A scrolling REPL, not a TUI.** No alt-screen: the transcript is the user's
    scrollback. The only owned region is a live area at the bottom (queued, one row
    per wrapped prompt line, an empty row, status) inside a `Viewport::Inline`. It is
    five rows of chrome plus the prompt, capped at half the screen: growing it pushes
    transcript lines into scrollback, shrinking leaves their rows blank until the
    next output fills them. A session chooser (`/sessions`, the startup picker) may
    also own one header row plus its entries, up to the screen minus the prompt and
    one margin row; when it closes without resuming, its rows are committed to
    scrollback, so the list survives and no blank rows are left. Everything above it
    is appended, never repainted.
13. **One writer.** Only `ui::Screen` touches the terminal. An event may mutate state
    and ask for output; nothing else prints. A stray `print!` is a rendering bug.
14. **The prompt is a block.** Every line of the prompt carries its glyph (the live
    `❯ `, the committed `█ `), so a wrapped line reads as one block, not as text
    running off the edge.
15. **`bash` is confined by the kernel, not by strings.** No child reaches the
    network, in either mode: seccomp refuses `socket` for the internet families on
    Linux, `(deny network*)` on macOS. Explore mode also denies every write outside
    a session scratch directory and every exec outside a fixed allowlist (Landlock
    on Linux, Seatbelt on macOS). Layer 1 is mandatory — a command that cannot be
    confined does not run; Layer 2 degrades with one startup line on a kernel
    without Landlock. A leading-word check is never a substitute. Accepted cost: no
    child TCP, ever — `git clone/fetch/push`, `ssh`, `gh`, `docker` and `cargo` with
    a cold cache are dead inside a tool call, and `!cmd` is no exception. Residuals:
    AF_UNIX `connect`, writes through an inherited fd, metadata (`chmod`) outside
    the scratch dir, and `fetch`, which runs in the parent by design.

## Architecture

- `main.rs` — REPL, provider setup, model picker, session lifecycle (startup
  chooser, `/sessions`, `/new`, naming), agent loop (unbounded, ends on no tool
  calls). Two producers (keys, a turn) feed one `mpsc` channel; the main thread
  applies events and draws once per batch.
- `ui.rs` — the screen: prompt buffer and key mapping, the live region (chrome plus
  the wrapped prompt, resized by `fit`), the session chooser, and `insert` for
  transcript lines (wrapped here, since a clipped line would be lost).
- `md.rs` — markdown: a port of `md.ts` (blocks, inlines, tables, entities, URL
  checks), plus the renderer that turns its AST into transcript lines. `Stream`
  feeds it a line at a time and lays a block out when it ends.
- `api.rs` — model listing, chat request, SSE parsing, tool-call accumulation.
- `http.rs` — URL parsing, HTTP/1.1 over `TcpStream`/rustls, body decoding
  (`Content-Length`, chunked, EOF).
- `config.rs` — load and validate the config; resolve the `agents` file.
- `state.rs` — the per-directory state file: the cwd key, load, and an atomic
  write. Its project directory also holds the sessions.
- `sessions.rs` — one meta sidecar and one append-only JSONL body per session;
  listing, full-text search, gzip for old bodies, and the UTC stamp.
- `tools.rs` — tool registry, JSON schemas, generated prompt, execution, and
  `fetch`/`web_search`: the host guard, the HTML-to-text tokenizer, the sanitizer,
  and the Brave/DuckDuckGo result parsers.
- `sandbox.rs` — the only `unsafe`: the seccomp filter and the Landlock ruleset on
  Linux, the `sandbox-exec` profiles on macOS, the exec allowlist, and the scratch
  directory explore mode may write to.

## Conventions

- Errors are `String`, user-facing, prefixed with context (`connect host:port: ...`).
  No error crates.
- Keep functions short; comment only non-obvious behavior.
- Transcript output is complete lines. `ui::Screen::text` splits on newlines and
  collects markdown; `ui::Screen::output` writes the binary's own text verbatim,
  one line per line.

Markdown is the model's and the user's, never the binary's. Blocks are laid out
when they end — a blank line, a tool call, a thought, or the end of the turn —
because a table needs its header and a paragraph its last line. The line still
being streamed stays raw in the status row.
- Style comes from `Paint` in `main.rs`, rendered either as cells or as ANSI when
  there is no screen to draw on.
- Tool names and descriptions are part of the prompt. Change them deliberately.
- Sessions live in the project state directory: `<id>.json` is a mutable sidecar
  (title, summary, dates, count, archived), `<id>.jsonl` is the append-only body of
  API messages. Bodies older than `sessions::KEEP` are gzipped to `<id>.jsonl.gz`;
  the sidecar stays readable, so listing and search work without unpacking, and a
  resumed session is unpacked back to plain. The system prompt and mode notes are
  never logged: resume rebuilds them from the current instructions and mode. A
  title and summary are generated once, after the first answer; failing that, the
  first request stands in.
- `!cmd` at the prompt is the user's own shell: it runs through `tools::execute`
  and its output joins the history as a `user` message, so the next turn sees it.
  No turn follows, explore mode's write confinement does not apply — the user typed
  it — and the network deny does, like every other child.
- Commit messages: imperative, one feature per commit.

## Provider notes

- `llm.local`: `http://llm.local:8000/v1`, plaintext, close-delimited bodies.
- DeepSeek: `https://api.deepseek.com`, `DEEPSEEK_API_KEY`. Model list reports
  `context_window`, not `context_length`.
- OpenCode Go: chat at `https://opencode.ai/inference/go/openai/v1`, models at
  `https://opencode.ai/inference/go/v1/models`, `OPENCODE_API_KEY`, requires a stable
  `x-opencode-session` header. Free-tier models return 403 outside OpenCode. Console
  API keys expire; an OAuth access token also works as a Bearer token.

## Build

```sh
cargo build
cargo run
```

No test suite yet. Verify changes with `./harness.py` (below): a pty plus a
virtual terminal is the only way to see what the user sees, and `cargo test`
cannot.

## Testing

`harness.py` is the loop. It starts `mock.py` in-process, writes a throwaway
config, runs the binary in a tmux pty of a fixed size, sends the keystrokes and
prints the final screen. Nothing is shared between runs; `--work DIR` makes one
directory (and its state) survive, which is how resume is tested.

```sh
./harness.py                              # one turn: text:ok
./harness.py "text:hi" "tool:read:src/ui.rs"
./harness.py --size 40x12 "blank:hi"
./harness.py "!a b c" "!C-u" "text:why"   # raw tmux keys, no Enter
./harness.py --keep "text:ok"             # keep the temp workdir
./harness.py --work /tmp/nano-s "text:first"
./harness.py --work /tmp/nano-s "!Down" "!Enter"   # resume the older one
```

Each positional argument is a turn: typed, then Enter. One starting with `!` is
a list of tmux key names sent as-is. `--ansi` keeps colors, `--scroll N` widens
the history in the dump, `--wait` and `--timeout` bound idling, `--bin` selects
the binary (built on first use). Mock replies are listed in `mock.py`.

`fetch:PAGE` has the mock call `fetch` on a fixture page (plain, inject, big,
image, redirect, latin1, slow); `PAGE@START+CHARS` sets the paging arguments. The
harness serves those pages on loopback and sets `NANO_FETCH_ALLOW_LOCAL=1` in the
child, since the host guard refuses loopback: that variable is a **test knob, off
by default, and a live bypass for anyone who sets it**. `--no-fetch-local` runs
without it, which is how the refusal is checked. `show:x` streams the last tool
result back, since the transcript shows only call lines.

`web_search` has no fixture: it asks fixed public hosts. The fake provider can
still call it, since `tool:` takes any name and arguments. That turn loops, so end
it with `!Escape` and read the result back; the first line says which engine
answered. The throwaway config binds `search.brave.api_key_env` to
`BRAVE_SEARCH_API_KEY`, so unset means DuckDuckGo and a bogus value exercises the
fallback:

```sh
./harness.py --wait 2 --scroll 400 'tool:web_search:{"query":"rust web framework"}' '!Escape' 'show:'
BRAVE_SEARCH_API_KEY=bogus ./harness.py --wait 2 --scroll 400 'tool:web_search:{"query":"rust"}' '!Escape' 'show:'
```

A `tool:` turn keeps calling the same tool: the mock answers from the last user
message, and a tool result is not one. Interrupt it with `!Escape`.

The sandbox is checked with the same harness. `tool:bash:{...}` takes JSON
arguments, and because that turn loops, end it with `!Escape` and read the result
back with `show:`; normal mode is a `!cmd` line, which does not loop:

```sh
./harness.py --scroll 400 'tool:bash:{"command":"ls | wc -l"}' '!Escape' 'show:'
./harness.py --scroll 400 'tool:bash:{"command":"echo x > /tmp/p"}' '!Escape' 'show:'
./harness.py --scroll 400 'tool:bash:{"command":"sed -n 1p f"}' '!Escape' 'show:'
./harness.py --scroll 400 'tool:bash:{"command":"git status --short"}' '!Escape' 'show:'
./harness.py --scroll 400 ' !python3 -c "import socket; socket.socket()"'
```

The leading space in the last line is load-bearing: the harness reads an argument
starting with `!` as tmux key names, and the REPL trims it back to a `!cmd`.

Requires python3 and tmux; no new crates.
