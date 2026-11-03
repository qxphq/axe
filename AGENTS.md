# nano

Minimal coding agent harness in Rust. One binary, a line REPL, OpenAI-compatible
providers, four tools. Roughly 4.5k LOC across seven files.

## Principles

1. **Dependencies are debt.** Prefer `std`. A crate must be strictly necessary and
   widely maintained. Current: `serde_json`, `rustls`, `webpki-roots`, `ratatui`
   (with its `crossterm` backend), `unicode-width`. `rustls` because crypto is never
   hand-rolled; `ratatui` because terminal geometry is the same class of problem —
   the hand-rolled strip renderer produced a new whitespace bug per fix.
2. **No speculative features.** Add a thing when its absence hurts. No alt-screen,
   panes, sessions, compaction, subagents, MCP, or permission layer until the REPL
   is genuinely insufficient. One feature per change.
3. **Small surface.** Seven modules. A new module needs a reason.
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
   prefix: it invalidates provider prompt caching. When sessions arrive: append-only
   JSONL, replay on resume, compaction last.
7. **Tools are dumb.** `read`, `write`, `edit`, `bash`. No `glob`, `grep`, or `list`;
   `bash` + `rg`/`fd` covers them. Tool errors are results for the model, not harness
   failures. Truncate output (30–50 KB), timeout `bash` (120 s).
8. **No approval prompts.** Bash runs unprompted by explicit user decision. Do not
   add a permission layer without being asked.
9. **Real cwd, relative paths.** `current_dir()` at startup, used for the prompt and
   every relative path. Never hardcode directories.
10. **One config file.** `~/.config/nano/config.json` (respects `XDG_CONFIG_HOME`),
    auto-created with an example on first run. Secrets only through `api_key_env`.
    Never commit keys, never read another app's auth store.
11. **Verify against reality.** Test locally against `mock.py` (chunked SSE,
    keep-alive comments, session headers). No speculative fixes; only fix what a
    reproduction shows.
12. **A scrolling REPL, not a TUI.** No alt-screen: the transcript is the user's
    scrollback. The only owned region is a live area at the bottom (queued, one row
    per wrapped prompt line, an empty row, status) inside a `Viewport::Inline`. It is
    five rows of chrome plus the prompt, capped at half the screen: growing it pushes
    transcript lines into scrollback, shrinking leaves their rows blank until the
    next output fills them. Everything above it is appended, never repainted.
13. **One writer.** Only `ui::Screen` touches the terminal. An event may mutate state
    and ask for output; nothing else prints. A stray `print!` is a rendering bug.
14. **The prompt is a block.** Every line of the prompt carries its glyph (the live
    `❯ `, the committed `█ `), so a wrapped line reads as one block, not as text
    running off the edge.

## Architecture

- `main.rs` — REPL, provider setup, model picker, agent loop (unbounded, ends on no
  tool calls). Two producers (keys, a turn) feed one `mpsc` channel; the main thread
  applies events and draws once per batch.
- `ui.rs` — the screen: prompt buffer and key mapping, the live region (chrome plus
  the wrapped prompt, resized by `fit`), and `insert` for transcript lines (wrapped
  here, since a clipped line would be lost).
- `md.rs` — markdown: a port of `md.ts` (blocks, inlines, tables, entities, URL
  checks), plus the renderer that turns its AST into transcript lines. `Stream`
  feeds it a line at a time and lays a block out when it ends.
- `api.rs` — model listing, chat request, SSE parsing, tool-call accumulation.
- `http.rs` — URL parsing, HTTP/1.1 over `TcpStream`/rustls, body decoding
  (`Content-Length`, chunked, EOF).
- `config.rs` — load and validate the config; resolve the `agents` file.
- `tools.rs` — tool registry, JSON schemas, generated prompt, execution.

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
- `!cmd` at the prompt is the user's own shell: it runs through `tools::execute`
  and its output joins the history as a `user` message, so the next turn sees it.
  No turn follows, and explore mode does not apply — the user typed it.
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
prints the final screen. Nothing is shared between runs.

```sh
./harness.py                              # one turn: text:ok
./harness.py "text:hi" "tool:read:src/ui.rs"
./harness.py --size 40x12 "blank:hi"
./harness.py "!a b c" "!C-u" "text:why"   # raw tmux keys, no Enter
./harness.py --keep "text:ok"             # keep the temp workdir
```

Each positional argument is a turn: typed, then Enter. One starting with `!` is
a list of tmux key names sent as-is. `--ansi` keeps colors, `--scroll N` widens
the history in the dump, `--wait` and `--timeout` bound idling, `--bin` selects
the binary (built on first use). Mock replies are listed in `mock.py`.

A `tool:` turn keeps calling the same tool: the mock answers from the last user
message, and a tool result is not one. Interrupt it with `!Escape`.

Requires python3 and tmux; no new crates.
