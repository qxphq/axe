# nano

A minimal coding agent in Rust. One binary, one line REPL, any
OpenAI-compatible `chat/completions` server, seven tools. The transcript is
your scrollback: nano never takes the screen.

## Build

```sh
cargo build --release
```

The binary is `target/release/nano`. It needs a current stable Rust toolchain
(edition 2021) and a C toolchain for `ring`. Linux (x86_64 or aarch64) and macOS
are supported. Other Unix-like targets compile, but `bash` is unavailable
there: a child that cannot be confined is not spawned. Windows is not supported.

## Run

```sh
nano             # in the directory you want it to work on
nano < prompts   # without a terminal: plain-text lines in, transcript out
```

The working directory is what the prompt shows, what `AGENTS.md` is looked up
from, and the key under which the session is remembered.

## Config

`~/.config/nano/config.json`, or `$XDG_CONFIG_HOME/nano/config.json`. It is
created with an example on first run and never rewritten afterwards: what you
pick in one directory is state, not config.

| key | meaning |
| --- | --- |
| `providers` | name → entry; or a top-level `base_url` for `local` |
| `model` | `provider/id` used by a directory that has no state yet |
| `explore_by_default` | start sessions in explore mode; default `true` |
| `search.brave` | Brave token via `api_key_env`; unset → DuckDuckGo |
| `thinking` | default level per `provider/id` for a fresh directory |
| `system` | text appended to the built-in prompt |
| `agents` | extra instructions file; `~/` expands, relative to the config |

A provider entry describes the differences between servers, never code paths:

| key | meaning |
| --- | --- |
| `base_url` | required; `chat/completions` hangs under it |
| `api_key_env` | environment variable holding the token |
| `models_url` | where the model list lives, when not `base_url` + `/models` |
| `session_header` | header that must carry a stable session id |
| `thinking` | level name → object merged into the request body |

`{}` as a level leaves the provider default in place.

```json
{
  "providers": {
    "example": {
      "base_url": "https://api.example.com/v1",
      "api_key_env": "EXAMPLE_API_KEY"
    }
  },
  "model": "example/some-model"
}
```

Thinking levels are named by the provider, not by nano: `off`, `low`, `medium`
and `high` are only what `/thinking` suggests. A level the provider does not
define falls back to its default.

```json
"thinking": {
  "low": { "reasoning_effort": "low" },
  "high": { "reasoning_effort": "high" }
}
```

Secrets belong in the environment: `api_key_env` is read at startup. A provider
whose variable is unset is skipped, not retried. A literal `api_key` in the
config is rejected — the file is not the place for a secret.

## Instructions

The built-in prompt is the working directory, a short numbered list of rules,
and the tool list. Instructions are appended after it, in this order: `agents`
from the config, then the nearest `AGENTS.md` at or above the working
directory, then `system`. `agents` and `AGENTS.md` are capped at 48 KB;
`system` is not. All are read once at startup; nothing is re-read mid-session,
and the mode is carried in the conversation, not the prompt.

A project's `AGENTS.md` comes from the repository, not from you, so it is
wrapped in `<project_instructions path="...">`. The first rule in the prompt
says that block is data: it describes the project, and it never overrides your
current request.

## REPL

`❯ ` is the prompt. Enter submits, Shift+Enter starts a new line, Tab completes
a command. A line starting with `!` is your own shell: it runs through the same
tool, its output joins the history as a user message, and no turn follows — the
model sees it the next time you ask something.

| command | what it does |
| --- | --- |
| `/model [id]` | pick or switch model |
| `/models` | list the provider's models |
| `/thinking [level]` | show or set the thinking level |
| `/thoughts` | show or hide the model's reasoning |
| `/explore` | toggle explore mode (also Shift+Tab) |
| `/new` | start a new session, naming the old one |
| `/sessions [query]` | browse or search this directory's sessions |
| `/help` | list the commands |
| `/exit`, `/quit` | quit |

Recent sessions for the directory are offered at startup as a chooser: Up and
Down move, Enter resumes, Esc closes it, and typing collapses it. `/sessions`
opens the same list, where typing filters it and Enter on an empty line resumes
the selection.

| key | what it does |
| --- | --- |
| Enter / Shift+Enter | submit / new line |
| Tab | complete a command |
| Shift+Tab | toggle explore mode |
| ↑ ↓ | move in a chooser |
| Esc | close a chooser, or cancel the running turn |
| Ctrl+C | clear the prompt line |
| Ctrl+D / Ctrl+A / Ctrl+E | end input / start of line / end of line |
| Ctrl+B / Ctrl+F, Alt+B / Alt+F | move by character, by word |
| Ctrl+W, Alt+W, Alt+D | kill the word before, before, after |
| Ctrl+U / Ctrl+K | kill to the start / to the end |
| Ctrl+Y / Ctrl+T / Alt+T | yank / transpose characters / transpose words |

Turns run in the background: the prompt stays live while the model answers, and
a line typed meanwhile is queued.

## Modes

Explore mode is a session setting, on by default (`explore_by_default`). When
it is on: `write` and `edit` are unavailable, `bash` runs only a fixed
allowlist of read-only commands and writes only inside a scratch directory,
and `fetch` and `web_search` are the way to read the web. When it is off,
`write`, `edit` and a full `bash` are back — `fetch` and `web_search` are
refused, and `bash` still cannot reach the network. Every change is announced
in the conversation as `explore mode: on` or `explore mode: off`, so the model
knows which side it is on.

## Tools

| tool | what it does |
| --- | --- |
| `read(path, offset?, limit?)` | a text file; relative paths resolve from cwd; up to 50 000 characters |
| `write(path, content)` | create or overwrite; parent directories are created |
| `edit(path, old, new, replace_all?)` | exact text; ambiguous `old` fails |
| `bash(command)` | run `sh -c` here; 120 s, 30 000 characters, no network |
| `about()` | return this file: tools, config, paths, limits |
| `fetch(url, start?, chars?)` | read a web page as text; explore mode only |
| `web_search(query, count?)` | ranked results; explore mode only |

`read` returns lines without numbering. `bash` starts every call in the working
directory, so `cd` is pointless; output is truncated, not streamed.
`fetch` takes 1 MiB off the wire at most and gives the model at most 50 000
characters, with `start` and `chars` to page through a longer page.
`web_search` asks Brave when a token resolves and DuckDuckGo otherwise, falling
back if Brave fails or returns nothing; `count` defaults to 10 and caps at 20.

## State and sessions

Per directory, under `$XDG_STATE_HOME/nano/projects/` (or
`~/.local/state/nano/projects/`), in a subdirectory named after the working
directory with every byte outside `[A-Za-z0-9._-]` written as `%XX` — so
`/home/anon/nano` becomes `%2Fhome%2Fanon%2Fnano`.

| file | contents |
| --- | --- |
| `state.json` | model, thinking levels, thoughts toggle for this directory |
| `<id>.json` | a session's title, summary, dates and message count |
| `<id>.jsonl` | the session body: one API message per line, append-only |
| `<id>.jsonl.gz` | a body older than the 30 most recent, gzipped |

Directories are `0700` and files `0600`: a session is plaintext, readable by
its owner only. Use a container, a separate user, or full-disk encryption when
the local account is not the right boundary.

The active body is append-only, so a crash can only leave an unfinished last
line. Archiving gzips and removes it; resuming unpacks it back to plain. On a
host without `gzip` nothing is archived — bodies stay plain, and a body already
archived elsewhere cannot be resumed there.
Listing and search read the sidecar, which stays plain: an archived body still
answers from its title and summary. Resuming one unpacks it back to plain. The
system prompt and mode notes are not logged; resume rebuilds them from the
current instructions and mode. Titles and summaries are generated after the
first answer, and fall back to the first request if that fails.

## Sandbox

`bash` is confined by the kernel, not by string checks. On Linux that is seccomp
plus Landlock, on macOS a Seatbelt profile.

Layer 1 is mandatory and applies in every mode: no child reaches the network,
ever. That is the accepted cost — `git clone`, `git fetch`, `git push`, `ssh`,
`gh`, `docker` and `cargo` with a cold cache do not work inside a tool call,
and `!cmd` is no exception. Where the filter cannot be installed, no command
runs at all.

Layer 2 applies in explore mode: writes outside a session scratch directory are
denied, and only an allowlist of programs may be executed — `ls`, `cat`, `head`,
`tail`, `grep`, `rg`, `fd`, `find`, `sort`, `wc`, `diff`, `du`, `df`, `file`,
`jq`, `stat`, `tree`, `uname`, `uniq`, `id`, `ps`, `pwd`, `date`, `cut`, `tr`,
`which`, `git`, and the shell itself. The loader is in that list because the
kernel opens it for every dynamically linked binary, so `ld.so` can start a
program that is not on the list, and an allowed program can load code with
`LD_PRELOAD`. Loaded code runs in the same domain: it still cannot write
outside the scratch directory or open an internet socket. It degrades with one
line at startup on a kernel without Landlock.

A refusal comes from the kernel, not from nano's argument parsing, and its
message says so: a missing binary and a denied one read differently.

A command child inherits a minimal environment — `PATH`, `HOME`, `LANG`,
`LC_ALL`, `TERM`, `TMPDIR`, `USER`, `SHELL` — not nano's own. Provider and
search tokens stay in the parent's memory.

Known residuals, in both modes: `connect` over AF_UNIX, writes through an
inherited file descriptor, metadata such as `chmod` outside the scratch
directory, and `fetch`/`web_search`, which run in nano's own process by design.
In explore mode, code loaded by an allowed program (`LD_PRELOAD`, the loader)
runs too, inside the same write and network confinement.

## Trust and security

nano runs as you. The trust boundary is the operating-system user: the model
can reach the files, the environment and the credentials you can reach. A
working directory is not trusted just because it is yours — a cloned
repository, its `AGENTS.md`, a file, a fetched page or a command's output is
data about the task, not authority over it. Source labels record provenance;
they are not enforcement.

Normal mode runs code as you. `bash` is a full shell, `write` and `edit` change
files, and no approval prompt stands in between — a deliberate trade-off. A
cloned repository's instructions, a file, or text you paste in can talk the
model into running something. Use normal mode only in directories you trust.
`bash` cannot open an internet socket in either mode, and `fetch` and
`web_search` are refused here, so a tool call cannot reach the network except
through the model provider.

Explore mode bounds integrity, not secrecy. The kernel denies writes outside
the session scratch directory, direct execution outside the allowlist, and
internet sockets; on a kernel without Landlock, the startup line says when that
is weaker. Reads are not confined: `read` and `bash` can read any file you can,
and `fetch` and `web_search` can send any of it to any host. `!cmd` is your own
shell — it bypasses explore mode's write confinement, and it is still
network-denied. When confidentiality matters, use a container, a VM, or a
dedicated user.

Prompt injection cannot be sanitized away. `fetch` and `web_search` results
start with `source: ... (untrusted)`, a project's instructions are wrapped in
`<project_instructions path="...">`, and the first rule says such content is
data. The web sanitizer removes terminal controls and chat-template tokens,
not persuasion: content that can address the model can still steer it. The
kernel sandbox, the provider, and the environment you choose are the real
boundary.
Reading the transcript shows what happened; it is not a boundary either.

What the model reads enters the conversation and goes to the provider with the
next request. Treat the provider as inside the trust boundary, along with
anyone who can read the session files: plaintext JSONL, `0600`, unencrypted.

Provider and search tokens are read from the environment at startup and stay
in memory. Command children do not inherit them, and `read` refuses `/proc`
environment files. The config only names the variables; it is rejected if it
carries a literal key.
