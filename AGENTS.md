# nano

Minimal coding agent harness in Rust. One binary, a line REPL, OpenAI-compatible
providers, four tools. Roughly 1.2k LOC across five files.

## Principles

1. **Dependencies are debt.** Prefer `std`. A crate must be strictly necessary and
   widely maintained. Current: `serde_json`, `rustls`, `webpki-roots`. TLS is the
   only justified exception; never hand-roll crypto.
2. **No speculative features.** Add a thing when its absence hurts. No TUI, sessions,
   compaction, subagents, MCP, or permission layer until the REPL is genuinely
   insufficient. One feature per change.
3. **Small surface.** Five modules. A new module needs a reason.
4. **Provider-agnostic through the OpenAI-compatible API.** `chat/completions` + SSE
   only. Never add per-provider code paths; express differences as config
   (`base_url`, `models_url`, `api_key_env`, `session_header`).
5. **The prompt is structure.** The built-in system prompt describes only the
   available tools and the working directory, and is generated from the same tool
   registry used for the `tools` array. No behavioral advice, no workflow rules.
   `system` from config is appended after it, never replaces it.
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
11. **Verify against reality.** Test with real providers. Use a local mock for
    protocol edge cases (chunked SSE, keep-alive comments, session headers). No
    speculative fixes; only fix what a reproduction shows.
12. **REPL over TUI.** Stream to stdout, read lines from stdin. Add `rustyline` only
    if missing history is the actual pain.

## Architecture

- `main.rs` — REPL, provider setup, model picker, agent loop (25 rounds).
- `api.rs` — model listing, chat request, SSE parsing, tool-call accumulation.
- `http.rs` — URL parsing, HTTP/1.1 over `TcpStream`/rustls, body decoding
  (`Content-Length`, chunked, EOF).
- `config.rs` — load and validate the config.
- `tools.rs` — tool registry, JSON schemas, generated prompt, execution.

## Conventions

- Errors are `String`, user-facing, prefixed with context (`connect host:port: ...`).
  No error crates.
- Keep functions short; comment only non-obvious behavior.
- Tool names and descriptions are part of the prompt. Change them deliberately.
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

No test suite yet. Verify changes against a real provider and the local mock for
protocol behavior.
