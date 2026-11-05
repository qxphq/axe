# axe

Minimal coding agent harness in Rust: one binary, a line REPL, OpenAI-compatible
providers, seven tools. The code is the source of truth; this file records the
invariants a change must not break.

## Principles

1. **Dependencies are debt.** Prefer `std`; a crate must be strictly necessary and
   widely maintained. Current: `serde_json`, `rustls`, `webpki-roots`, `ratatui`
   (crossterm backend), `unicode-width`, and `libc` on Linux. Crypto and terminal
   geometry are not hand-rolled; compression is the host's `gzip`, never a crate.
2. **No speculative features.** Add a thing when its absence hurts. No alt-screen,
   panes, compaction, subagents, MCP, or permission layer until the REPL is
   genuinely insufficient. One feature per change.
3. **Small surface.** Twelve modules; a new module needs a separate concern, not
   a line count.
4. **Provider-agnostic through the OpenAI-compatible API.** `chat/completions` +
   SSE only. Express differences as config (`base_url`, `models_url`,
   `api_key_env`, `session_header`), never per-provider code paths.
5. **The prompt is structure.** The built-in system prompt is cwd, a short numbered
   list of rules that hold for every user, and the tools from the same registry as
   the `tools` array. Rules are policy: one line each, no workflow, nothing
   project- or provider-specific. Instructions are appended in order: `agents`
   (config), the nearest `AGENTS.md` at or above cwd, then `system`, then the
   skills catalogue. The project file is wrapped in
   `<project_instructions path="...">` and the first rule calls it data, not
   authority. Read once at startup; never re-read mid-session.
6. **Stable prefix, append-only history.** System prompt and tool schemas first,
   volatile content last. Never inject timestamps, status, or cwd changes into the
   prefix: it invalidates provider prompt caching. Sessions are append-only JSONL,
   replayed on resume; compaction stays last.
7. **Tools are dumb.** `read`, `write`, `edit`, `bash`, `about`, `fetch`,
   `web_search`. No `glob`, `grep`, or `list`; `bash` + `rg`/`fd` covers them.
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
   read anyway, so it buys nothing but friction.
9. **Real cwd, relative paths.** `current_dir()` at startup, used for the prompt and
   every relative path. Never hardcode directories.
10. **One config file, and it is input.** `~/.config/axe/config.json` (respects
    `XDG_CONFIG_HOME`), auto-created on first run. Secrets only through
    `api_key_env`; never commit keys or read another app's auth store. Axe never
    writes it: the per-directory state and sessions live under
    `~/.local/state/axe/projects/<escaped cwd>/` (respects `XDG_STATE_HOME`).
    Config `model` is the default for a directory with no state.
11. **Verify against reality.** Test against `mock.py` (chunked SSE, keep-alive,
    session headers). No speculative fixes; reproduce first.
12. **A scrolling REPL, not a TUI.** No alt-screen: the transcript is scrollback.
    The only owned region is an inline live area at the bottom (queued row,
    wrapped prompt, the candidate list under it, status), capped at half the
    screen. A chooser may own a header plus entries; when it closes without
    resuming, its rows are committed to scrollback. Everything above is
    appended, never repainted — except `/new`, which wipes the screen and its
    scrollback: a session boundary.
13. **One writer.** While `ui::Screen` owns the terminal, only it draws. Startup
    banner and piped output run before/without a `Screen`; a stray `print!` while
    it exists is a rendering bug.
14. **The prompt is a block.** Every line carries its glyph (`❯ ` live, `█ `
    committed), so a wrapped line reads as one block.
15. **`bash` is confined by the kernel, not by strings.** Explore mode denies
    writes outside a scratch directory and exec outside a fixed allowlist
    (Landlock/Seatbelt, plus seccomp denying `io_uring_setup`, which Landlock
    cannot mediate); it degrades with one startup line. The allowlist is a
    convenience guard, not the containment: it carries the common inspection and
    tooling set, interpreters and package managers included, and leaves out
    daemons that write through another process (`docker`, `systemctl`), setuid,
    and mode/timestamp tools (`chmod`, `touch`), which Landlock cannot see.
    Normal mode keeps only `no_new_privs` and the scrubbed environment. The
    network is confined in neither mode: a child connects where the user
    connects, `!cmd` included. A leading-word check is never a substitute.
    Details and residuals: `SEC_AUDIT.md`.

## Architecture

- `main.rs` — REPL, provider setup, model picker, session lifecycle (startup
  chooser, `/sessions`, `/new`, naming), agent loop. Two producers (keys, a turn)
  feed one `mpsc` channel; the main thread applies events and draws once per batch.
- `ui.rs` — the screen: prompt buffer and key mapping, the live region (chrome,
  the wrapped prompt, the command candidates under it, resized by `fit`), the
  chooser, and `insert` for transcript lines (wrapped here, since a clipped line
  would be lost).
- `markdown.rs` — markdown: blocks, inlines, tables, entities, URL checks, and the
  renderer. `Stream` feeds it a line at a time and lays a block out when it ends.
- `api.rs` — model listing, chat request, SSE parsing, tool-call accumulation.
- `http.rs` — URL parsing, HTTP/1.1 over `TcpStream`/rustls, body decoding
  (`Content-Length`, chunked, EOF), line/body caps and timeouts.
- `config.rs` — load and validate the config; resolve the `agents` file.
- `state.rs` — per-directory state: the cwd key, load, and an atomic write.
- `sessions.rs` — meta sidecar and append-only JSONL body per session; listing,
  search, system gzip for old bodies, UTC stamp.
- `tools.rs` — tool registry, schemas, generated prompt, local tools, `about`.
- `web.rs` — `fetch`/`web_search`: host guard, HTML tokenizer, sanitizer,
  Brave/DuckDuckGo parsers, paging.
- `skills.rs` — Agent Skills discovery, the frontmatter subset, the catalogue,
  and the body read on `/skill:` activation.
- `sandbox.rs` — the only `unsafe`: seccomp, Landlock, Seatbelt, exec allowlist,
  and the explore-mode scratch directory.

## Conventions

- Errors are `String`, user-facing, prefixed with context (`connect host:port: ...`).
  No error crates.
- Keep functions short; comment only non-obvious behavior.
- Transcript output is complete lines. `Screen::text` collects markdown;
  `Screen::output` writes the binary's own text one line per line.
- Markdown is the model's and the user's, never the binary's. Blocks are laid out
  when they end; the live row says what the turn is doing, not the unfinished line.
- Style comes from `Paint` in `main.rs`, rendered as cells or ANSI.
- Tool names and descriptions are part of the prompt. Change them deliberately.
- `README.md` is the user-facing contract and the GitHub page. It ships through
  `about`, so keep it factual and in the same commit as the behavior it describes.
- Skills: a directory with `SKILL.md` is the unit; only the frontmatter is read at
  startup, the body on demand. The catalogue is appended last in the system
  prompt, after the instructions, so the part above it stays byte-identical.
  Project roots stop at the repository root; without a `.git`, only cwd counts.
- Sessions: `<id>.json` is the mutable sidecar; `<id>.jsonl` is append-only; bodies
  older than `sessions::KEEP` are gzipped. The sidecar stays readable, so listing
  and search work without unpacking. The system prompt and mode notes are never
  logged; resume rebuilds them. A title and summary are generated once, after the
  first answer; failing that, the first request stands in.
- `!cmd` is the user's shell. It runs through `tools::execute`, its output joins
  the history as a `user` message, no turn follows, and explore-mode write
  confinement does not apply.
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

`fetch:PAGE` has the mock call `fetch` on a fixture page (plain, inject, big,
image, redirect, latin1, slow); `PAGE@START+CHARS` sets paging. The harness serves
those pages on loopback and sets `AXE_FETCH_ALLOW_LOCAL=1`; that variable is a
**debug-build test knob only, off by default; release builds ignore it**.
`--no-fetch-local` checks the refusal. `show:x` streams the last tool result back.

`web_search` asks fixed public hosts; `tool:` can call it. A `tool:` turn keeps
calling the same tool, so end it with `!Escape` and read the result with `show:`;
the first line says which engine answered. `BRAVE_SEARCH_API_KEY` selects Brave;
unset means DuckDuckGo, and a bogus value exercises the fallback.

Skills are checked with a `.agents/skills/<name>/SKILL.md` under `--work DIR`:
`/skills` lists it and `/skill:name` runs the body. The sandbox is checked the
same way: `tool:bash:{...}` takes JSON arguments and loops; end with `!Escape`
and `show:`. Normal mode is a `!cmd` line, which does not loop. Examples:

```sh
./harness.py --scroll 400 'tool:bash:{"command":"head -n 1 tmux.conf"}' '!Escape' 'show:'
./harness.py --scroll 400 'tool:bash:{"command":"echo x > /tmp/p"}' '!Escape' 'show:'
./harness.py --scroll 400 'tool:bash:{"command":"python3 -c pass"}' '!Escape' 'show:'
```

The first two deny on the kernel: an unwritable path, an exec outside the
allowlist. A `!cmd` line is the escape hatch, and the harness reads an argument
starting with `!` as tmux keys, so prefix it with a space: `' !make'`.
