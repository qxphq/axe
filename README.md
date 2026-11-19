# axe

```text
 ▄▄▄  ▄▄ ▄▄ ▄▄▄▄▄
██▀██ ▀█▄█▀ ██▄▄
██▀██ ██ ██ ██▄▄▄
```

A minimal coding agent in Rust. One binary, one line REPL, any
OpenAI-compatible `chat/completions` server, seven tools, **six dependencies**.
It does not re-implement your shell, your tools or your system.

Sessions start in explore mode (shown as `read-only`), where `fetch` and
`web_search` are available and the writing tools are not.

## Install

```sh
curl -fsSL https://raw.githubusercontent.com/qxphq/axe/main/install.sh | sh
```

That fetches a build of `main` from the rolling `latest` release, checks its
sha256 and installs it into `~/.local/bin` (`AXE_INSTALL_DIR` overrides). Assets
exist for x86_64 and aarch64 Linux (glibc; on musl, Alpine included, the script
says so and stops) and for Apple Silicon and Intel macOS. The release is
rebuilt on every push to `main`, so its version is whatever `Cargo.toml` says at
that commit — no tag marks it.

On NixOS it installs the flake instead — `nix profile add
--no-write-lock-file github:qxphq/axe#default` — which builds from source and
wraps what it runs: `sh`, `git`, `gzip`, coreutils' `kill` and `sha256sum`, and
the `rg` and `fd` a confined `bash` needs.

## Run

```sh
axe              # in the directory you want it to work on
axe < prompts    # without a terminal: plain-text lines in, transcript out
axe --unsafe     # -u: start with write, edit and a full bash (explore mode off)
axe --version    # prints the version and exits; no config, no network
axe --help       # prints the usage and exits; no config, no network
```

`--unsafe` (`-u`) overrides `explore_by_default` for the session that starts: a pipe
gets a normal-mode session from its first line, and `/new` still turns explore
mode back on, because the mode belongs to the session.

An argument that is neither is named, and axe exits with the usage rather than
ignoring it.

Without a terminal the line comes from stdin and stdout carries the model's
text: no splash, no selection, no prompt and no notice, so `axe < prompts` is a
filter. A tool call is axe's own account of the turn and goes to stderr, so
`axe < prompts 2>/dev/null` is the answers alone; what a line explicitly asks
for (`!cmd`, `/sessions`, `/model`) still prints to stdout. Nothing that needs
a screen exists there — `/sessions` prints a numbered list and the line that
says how to resume one, and the startup chooser does not appear.

The working directory is what the prompt shows, what `AGENTS.md` is looked up
from, and the key under which the session is remembered.

The version follows semver and is written in exactly one place, `Cargo.toml`:
the banner and `--version` read it at compile time, and `flake.nix` reads the
same file. The `ship` skill bumps it with the change.

## Config

`~/.config/axe/config.json`, or `$XDG_CONFIG_HOME/axe/config.json`. It is
created with an example on first run and never written afterwards: what you
pick in one directory is state, not config, and so is `/model -g`'s default.

| key | meaning |
| --- | --- |
| `providers` | name → entry; or a top-level `base_url` for `local` |
| `model` | `provider/id` a directory starts on when neither it nor the state root has picked one |
| `explore_by_default` | start sessions in explore mode; default `true` |
| `search.brave` | Brave token via `api_key_env`; unset → DuckDuckGo |
| `thinking` | default level per `provider/id` for a fresh directory |
| `system` | text appended to the built-in prompt |
| `agents` | extra instructions file; `~/` expands, relative to the config |
| `skills` | extra skill directories or `SKILL.md` files |

A provider entry describes the differences between servers, never code paths:

| key | meaning |
| --- | --- |
| `base_url` | required; `chat/completions` hangs under it |
| `api_key_env` | environment variable holding the token |
| `models_url` | where the model list lives, when not `base_url` + `/models` |
| `session_header` | header that must carry a stable session id |
| `session_field` | body field that carries the same id, for a provider that routes its cache by it (`prompt_cache_key`) |
| `max_tokens` | tokens an answer may take; `16384` unless set, `0` for no ceiling |
| `max_tokens_field` | the field that ceiling travels in (`max_tokens`, or `max_completion_tokens`) |
| `store` | whether the provider may retain the request (`false` opts out) |
| `thinking` | level name → object merged into the request body |

The file is read strictly: a key it does not define, or a known key of the
wrong type, is an error naming the file and the key, so a typo cannot change
what a session does in silence. `providers` and a top-level `base_url` say the
same thing, so a file with both is refused. A provider name is the first half
of a `provider/id`, so it cannot be empty or hold `/` or whitespace.

```json
{
  "providers": {
    "example": {
      "base_url": "https://api.example.com/v1",
      "api_key_env": "EXAMPLE_API_KEY",
      "thinking": { "low": { "reasoning_effort": "low" } }
    }
  },
  "model": "example/some-model"
}
```

A thinking level is merged on top of its entry's fields, so `max_tokens` in a
level is a per-level ceiling; `{}` leaves the provider default in place.
`session_field` and `store` are the cache and retention fields sent when a
provider supports them: axe sends nothing it was not told to, because a
provider that rejects an unknown body field would fail every request. Levels
are named by the provider, not by axe — `off`, `low`, `medium` and `high` are
only what `/thinking` offers — and a level the provider does not define falls
back to its default.

Secrets belong in the environment: `api_key_env` is read at startup, a provider
whose variable is unset is skipped rather than retried, and a literal `api_key`
in the config is rejected.

The splash is printed before any provider is asked for its model list; the
lists are asked in parallel, each answer adding a `name: N models` line, so a
slow network reads as work rather than as a blank terminal. A provider has 10 s
to answer — one that does not is skipped with a line — and the MCP servers are
started the same way.

MCP servers are a second input file, `mcp.json` beside this one, read the same
way and never part of it; see [MCP servers](#mcp-servers).

## Instructions

The built-in prompt is the working directory, a paragraph on what each mode
allows, and a short numbered list of rules. Tool descriptions and schemas are
sent only in the request's `tools` array; a tool is named in the prompt only
where it has to be exact — the ones a mode takes away, and `about`.
Instructions are appended in this order: `agents` from the config, then the
nearest `AGENTS.md` at or above the working directory, then `system`, then the
skills catalogue. `agents` and `AGENTS.md` are capped at 48 KB, and a file over
the cap ends with a line that says so; `system` is not capped.

All of it is read once at startup and never re-read mid-session; the mode is
carried in the conversation, not the prompt. A project's `AGENTS.md` comes from
the repository, not from you, so it is wrapped in
`<project_instructions path="...">`, and the first rule says that block is
data: it describes the project and never overrides your current request.

## Skills

A skill is a directory containing `SKILL.md`: YAML frontmatter (`name`,
`description`) followed by instructions. axe implements the Agent Skills
format (`agentskills.io`); relative references in the body resolve against the
skill's directory. Only the name and description are loaded at startup, so a
`SKILL.md` written during a session is picked up on the next start — which is
what `/skills` and an unknown `/skill:name` say.

| scope | path |
| --- | --- |
| project | `.agents/skills/` in the working directory and each ancestor up to the repository root |
| global | `<config dir>/skills/`, then `~/.agents/skills/` |
| config | each `skills` path; `~/` expands, relative to the config |

Directories are scanned recursively; a directory holding `SKILL.md` is a skill
and is not descended into. The first name wins, so a project skill replaces a
global one. `.git`, `node_modules`, `target` and dot directories are skipped; at
most 200 skills and 16 KB of catalogue are used. There is no trust prompt: a
directory you would not trust is a directory you should not run a coding agent
in.

`/skills` lists what was found and where it came from.
`/skill:name request` loads a skill explicitly and runs a turn; `/skill:name`
completes with Tab. `allowed-tools` is parsed and shown but not enforced — axe
has no approval layer to pre-approve against — and
`disable-model-invocation: true` keeps a skill out of the model's catalogue
while `/skill:name` still works.

The catalogue is the last section of the system prompt: each visible skill's
name, description and absolute `SKILL.md` path, which the model reads when a
task matches.

## MCP servers

axe can run [MCP](https://modelcontextprotocol.io) servers and add their tools
beside `read` and `bash`. Two transports, tools only — resources, prompts and
`tools/list_changed` are not implemented — and no server is configured by
default.

| | global | project |
| --- | --- | --- |
| file | `~/.config/axe/mcp.json` (or `$XDG_CONFIG_HOME/axe/mcp.json`) | `<repo>/.mcp.json`: the nearest at or above cwd, never past the repository root |
| applies to | every directory | that checkout only |
| written by | `/mcp add <name> ...` | `/mcp add --project <name> ...` |
| approval | none: it is your file | once per file, per content hash; the question lists the commands and the variables they read |
| precedence | — | a project server replaces a global one of the same name |

```json
{ "mcpServers": {
    "github": { "command": "npx",
                "args": ["-y", "@modelcontextprotocol/server-github"],
                "env": { "GITHUB_TOKEN": "${GITHUB_TOKEN}" } },
    "sentry": { "url": "https://mcp.sentry.dev/mcp" },
    "grep":   { "url": "https://mcp.grep.app",
                "headers": { "Authorization": "Bearer ${GREP_KEY}" } },
    "docs":   { "command": "/opt/mcp-docs", "enabled": false, "timeout": 120 } } }
```

| key | meaning |
| --- | --- |
| `command` | a program axe starts and speaks to over its stdin/stdout |
| `url` | a streamable-HTTP endpoint; an entry is a command or a url, not both |
| `args` | strings for a command; `~` is your home directory, and `$NAME` expands at start |
| `env` | for a command: name → value, or an array of names as shorthand |
| `headers` | for a url: name → value |
| `enabled` | `false` keeps the entry and starts nothing |
| `timeout` | seconds per call; 60 by default |

Values are strings with `$NAME` and `${NAME}` expanded from axe's own
environment at start, `$$` for a literal `$`: that is how a committed file
names a secret without holding one. A missing variable is reported by name and
the server does not start half-configured. `type` may name `stdio`, `http` or
`streamable-http`; unknown keys are errors. An entry with no `command`/`url`
may only switch a global server's `enabled` — a repository can narrow what
runs, never define it.

A `url` entry is streamable HTTP: axe POSTs JSON-RPC, accepts a JSON body or an
SSE stream, echoes `Mcp-Session-Id`, sends `MCP-Protocol-Version`, and DELETEs
the session when it closes. Authentication is the bearer token a `headers`
entry names; axe sends nothing it was not given, reads no other auth store, and
reports a 401 as the server's answer.

Tools are named `mcp__<server>__<tool>` and carry the server's own schema and
description. They are read once at startup, because the catalogue is part of
the cached prompt prefix: an edit needs a restart, and `/mcp` says when a file
changed. At most 128 tools and 256 KiB of schema per server, 4 MiB per message,
and 30 000 characters per result with the whole text in a temporary file the
result names. The model gets text: an image or a resource is `[image omitted]`.
ESC stops waiting (the process keeps its server; an HTTP request is closed); a
server that exits answers with what it printed.

```
$ /mcp
mcp servers
  github     global   stdio  26 tools
  proj       project  stdio  4 tools
  /home/anon/repo/.mcp.json: approved
```

`/mcp <name>` shows one server's tools. `/mcp add [--project] [--url URL]
[--env NAME[=VALUE]]... [--header 'K: V']... [--timeout S] <name> [command
[args...]]`, `/mcp remove <name>` and `/mcp forget` edit the files, and the
startup line is the proof that it worked: `mcp github: 26 tools (global)`.
`--env NAME` writes a reference (`${NAME}`); `--env NAME=VALUE` writes the value
as given, so a secret belongs in the first form — a project file is committed.
Either command keeps an approval the file already had and never creates one it
did not: an entry added to a file nobody approved still asks at the next start,
about the whole file.

In explore mode a stdio server is confined like `bash` — the same two writable
directories, the same `io_uring` rule, exec unconfined; see
[Sandbox](#sandbox). A process cannot be narrowed, so `/explore` restarts every
server: the names the model was given stay the same, and one that fails then
answers with an error.

## REPL

The line sits in a box: a cyan outline one cell in from each side, yellow while
explore mode takes the writing tools away, one row per line the text wraps to.
Enter submits; Ctrl+J or Alt+Enter starts a new line, and the box grows with
every row it takes, scrolling inside itself past the screen.

Typing a slash word lists the matching commands above the prompt, up to four,
the first marked. Matching is fuzzy — the characters typed, in order, gaps
allowed — so `/mdl` lists `/model`. ↓ and ↑ move the mark and leave the line as
typed, wrapping at both ends; Enter runs the marked row and Tab puts it on the
line, so a step never rewrites the prompt by itself.

A path in a message is not something to run: its list shows the path being
completed rather than the message around it, only Tab writes one onto the line,
and Enter submits the line exactly as it was typed.

`/model` and `/thinking` are the exception: their argument is not completed,
because Enter would take a candidate before the command ran. Bare, they open a
chooser over what they pick from — every provider's models, or the levels this
one configures — opened on the one in use, walked with the arrows, which stop
at the ends, narrowed by typing, closed with Esc, applied on Enter. `/model
dee` is the same chooser, already narrowed; `/model provider/id` sets one
directly, and `-g` (`--global`) is the same pair for the global default.

A line starting with `!` is your own shell: it runs through the same tool, its
output joins the history as a user message, and no turn follows — the model
sees it the next time you ask something. It leaves the line a call leaves
(`▷ cmd ✓`, or `✗` when it failed), the command's own words under it and
the exit status carried by the glyph.

Every submitted line is remembered for the directory, across sessions: ↑ walks
what was typed here, newest first, ↓ comes back down, and Ctrl+R searches. A
blank line, a line starting with a space (the shell's "do not remember this")
and a repeat of the line before it are not remembered.

Tab completes paths, and is the only key that does. `@` starts one anywhere in
the line: `@READ` completes to `README.md`, and what the model reads is the
path, not a sigil. Without it only
the line's first token completes, and only when it is a path — one that starts
with `~`, `/` or `.`, or holds a `/` — so a command is typed from `/` and a
message about `/etc/nix` is left alone. Directories come first with a trailing
`/`, so Tab on one keeps the list open and a lone file ends it with a space. A
path with a space in it is not completed.

| command | what it does |
| --- | --- |
| `/model [id]` | switch to a model; bare, a chooser over every provider's models, narrowed by typing; without a terminal it only lists |
| `/model -g [id]` | set the global default: the model a directory that pins none starts on; same chooser; the directory's own choice is dropped so it follows |
| `/thinking [level]` | show or set the thinking level; bare, a chooser over the levels the provider configures |
| `/thoughts` | show or hide the model's reasoning |
| `/explore` | toggle explore mode (also Shift+Tab) |
| `/new` | start a new session, naming the one it left, clearing the screen, and turning explore mode back on |
| `/sessions [query]` | browse or search this directory's sessions |
| `/compact` | summarize the session and continue from the summary |
| `/skills` | list loaded skills, where they came from, and what the model sees |
| `/mcp [name]` | the MCP servers: list them, one server's tools, `add`, `remove`, `forget` |
| `/skill:name [request]` | load a skill's instructions and run a turn |
| `/help` | list the commands, the key bindings, the config path and its keys |
| `/exit`, `/quit` | quit |

The area under the transcript is as tall as it has to be — candidates, queued
lines, the prompt's rows, the status — and its first row is blank whatever it
shows, so nothing it draws touches the block above it. A chooser is the one part
sized by the terminal: a window over the transcript, whose rows are committed
when it closes.

The left of the row says what the turn is doing — thinking, running a command,
responding — blank only when nothing is in flight, and the states you wait on or
interrupt carry a glyph: `⋯` waiting for the provider and then thinking, `▸`
responding, `▷` a command running, `▣` compacting, `×` interrupting (and a
cancelled compaction), while `◦` leads axe's own notices (`Resumed:`, `New
session`, `compacted N messages`, `nothing to compact`, `Default model:`). On
the right: the session's name, the directory (home written `~`), the
provider/model, the thinking level, and how much of the context window the
request fills — the provider's own count once a round reports one plus an
estimate of what came after it, with the cache hit beside it
(`412/8192 (5%) · 390 cached`) — colored by the theme's own ANSI names, the
window green until 70% full, yellow to 90%, then red, and `read-only` yellow. A
wide row clips nothing but a title that does not fit; only a status long enough
to fill the row holds it to half. The name is the first request until the model
names the session after the first answer.

Recent sessions are offered at startup as a chooser: Up and Down move, Enter
resumes, Esc closes it, and typing starts a new session and takes the list with
it. A directory with nothing to resume gets one dim line instead: the mode,
`/help`, and the two keys nothing else on screen names — `↑` for this
directory's history and Tab for completion. `/sessions` opens the same list,
where typing filters it and Enter on an empty line resumes the selection.

| key | what it does |
| --- | --- |
| Enter | submit the line; over a command list, run the marked command |
| Ctrl+J, Alt+Enter | new line |
| Tab | complete a command or a path token: put the marked candidate on the line without submitting it |
| ↑ ↓ | move the mark over the candidates above the prompt, walk the history when there is no list, or move in a chooser; ↑ with lines waiting takes them back |
| Shift+Tab | toggle explore mode |
| Esc | close a chooser; twice to interrupt the running turn |
| Ctrl+C | clear the prompt line |
| Ctrl+D / Ctrl+A / Ctrl+E | end input / start of line / end of line |
| Ctrl+B / Ctrl+F, Alt+B / Alt+F | move by character, by word |
| Ctrl+W, Alt+W, Alt+D | kill the word before, before, after |
| Ctrl+U / Ctrl+K | kill to the start / to the end |
| Ctrl+R | search the history backwards: the line previews the match, Enter accepts, Esc gives the line back |
| Ctrl+Y / Ctrl+T / Alt+T | yank / transpose characters / transpose words |

Turns run in the background: the prompt stays live while the model answers, and
the lines typed meanwhile are queued, one row each above the prompt. Each joins
the turn at the next round boundary — after the tool calls the last answer
asked for have run, before the next request — so the model reads it while it
still works. `↑` takes them all back into the prompt, in order and above
whatever was typed, and empties the queue: a line the turn has already read
cannot be called back, and its echo stays in the transcript. A command
(`/model`, `/sessions`, `!cmd`) needs the prompt and the session, so it waits
for the turn to end.

Interrupting takes two presses — for a turn, a `!cmd` and a `/compact` alike:
the first puts the offer on the row under the prompt, any other key withdraws
it, and the second stops what is running. A stopped turn says so in the
transcript, because a cut reply reads like a finished one; a stopped command
shows its own words, and a cancelled compaction its notice. Ctrl+D in the
prompt stops a running turn the same way. A reply the provider cut at its own
limit, or one whose stream died before its end, gets the same line beneath it:
the text that arrived is the model's and stays in the session, and the line
says the round was not whole.

## Modes

Explore mode is a session setting, on by default (`explore_by_default`), and
shown as `read-only`. When it is on, `write` and `edit` are unavailable, `bash`
runs only a fixed allowlist of commands and writes only inside a scratch
directory and the project's `plan/`, and `fetch` and `web_search` are the way
to read the web. When it is off, `write`, `edit` and a full `bash` are back,
and `fetch` and `web_search` are refused. `--unsafe` (`-u`) starts the session that
flag opens in the off side, whatever the config says. `/new` turns explore mode
back on: the mode belongs to the session, and the next one starts read-only.
Every flip is announced in the conversation as `explore mode: on` or `explore
mode: off`, so the model knows which side it is on.

## Tools

| tool | what it does |
| --- | --- |
| `read(path, offset?, limit?)` | a text file; relative paths resolve from cwd; up to 50 000 characters |
| `write(path, content)` | create or overwrite; parent directories are created |
| `edit(path, old, new, replace_all?)` | exact text; ambiguous `old` fails |
| `bash(command)` | run `sh -c` here; 120 s, 30 000 characters |
| `about()` | return this file: tools, config, paths, limits |
| `fetch(url, start?, chars?)` | read a web page as text; explore mode only |
| `web_search(query, count?)` | ranked results; explore mode only |

An MCP server's tools join the same array as `mcp__<server>__<tool>`; see
[MCP servers](#mcp-servers). The seven above are always axe's own and never
move behind a server.

`read` returns lines without numbering, joined with LF: a CRLF file reads as
LF, and an `edit` copying what it saw has to match the bytes on disk. A window
over the cap stops after a whole line and names the offset that continues it;
one line longer than the cap cannot be paged by line, and says so. An argument
of the wrong type is an error, never a default, so a call that asked for line
12 is never answered with line 1.

`bash` starts every call in the working directory, so `cd` is pointless. Its
output over 30 000 characters is cut, not streamed, and the result names an
owner-only file holding each whole stream: the middle of a long log is usually
where a build failed, and re-running the command that wrote it may not be safe.

`fetch` takes 1 MiB off the wire at most and gives the model at most 50 000
characters, with `start` and `chars` to page through a longer page; a body it
had to cut says so. An HTML page is reduced to text, and any other body — plain
text, JSON, XML, markdown — is read as the text it is. `web_search` asks Brave
when a token resolves and DuckDuckGo otherwise, falling back if Brave fails or
returns nothing; `count` defaults to 10 and caps at 20.

## State and sessions

Per directory, under `$XDG_STATE_HOME/axe/projects/` (or
`~/.local/state/axe/projects/`), in a subdirectory named after the working
directory with every byte outside `[A-Za-z0-9._-]` written as `%XX` — so
`/home/anon/axe` becomes `%2Fhome%2Fanon%2Faxe`.

| file | contents |
| --- | --- |
| `state.json` | model, thinking levels, thoughts toggle |
| `history.jsonl` | prompt lines typed here, one JSON string per line; the newest 500 |
| `mcp-trust.json` | approved project MCP files, by path and content hash |
| `plan/` | explore mode's writable directory: a plan that outlives the session |
| `sessions/` | one pair per session, below |
| `sessions/<id>.json` | a session's title, summary, dates and message count |
| `sessions/<id>.jsonl` | the session body: one API message per line, append-only |
| `sessions/<id>.jsonl.gz` | a body older than the 30 most recent, gzipped |

`$XDG_STATE_HOME/axe/default.json` (or `~/.local/state/axe/default.json`)
holds the global default model: what `/model -g` writes, and what a directory
whose `state.json` names no model follows, before the config's `model`.

Directories are `0700` and files `0600`: a session is plaintext, readable by
its owner only. With neither `XDG_STATE_HOME` nor `HOME` set, axe says so at
startup and keeps nothing — no sessions, prompt history, state or MCP approval —
rather than write them under the working directory. Use a container, a separate user, or full-disk encryption when
the local account is not the right boundary.

The active body is append-only, so a crash can only leave one unfinished line:
it is skipped when the body is read, and the next message starts a line of its
own, so nothing written after the crash is lost to the next resume. A crash
between a tool call and the result that answers it leaves a gap a provider
refuses — an assistant message naming an id no later message answers — so
reading a body answers every call it left open, and drops a result whose call
is not there, before the session replays. The body is not rewritten: the repair
is what the session carries, and the next read gives the same one. Archiving
gzips and removes a body and resuming unpacks it back to plain; on a host
without `gzip` nothing is archived, and a body archived elsewhere cannot be
resumed there. Listing and search read the sidecar, which stays plain, so an
archived body still answers from its title and summary. The system prompt and
mode notes are not logged; resume rebuilds them from the current instructions
and mode. Titles and summaries are generated after the first answer, and fall
back to the first request if that fails. A session body belongs to one running
axe: two instances in one directory share the selection last-write-wins, and an
archive pass skips a body another instance has touched since it was listed.

`/compact` replaces the history with a summary of it, and so does axe on its
own when the request comes within 16384 tokens of the window: the summary is
made before the request rather than after the provider refused it. The newest
conversation — up to 20000 tokens, cut at a user or assistant message so a tool
result never loses the call it answers — stays verbatim beside the summary, and
a history that fits inside that tail is summarized whole. The body is not
rewritten: the summary is appended as a marked message with the tail after it,
and reading a body starts from the last marker, so replay matches what the live
session sent. The summary is printed once, as its own block under the notice
that records the compaction, and a resume does not print it again. What came
before the marker stays on disk and `/sessions` search still finds it, so a
compacted session lists the messages the model has, not the bytes in the file.

The summary and the naming request are the only requests without tools: they
run at the session's thinking level, carry the entry's cache and retention
fields, and have no output cap, which a thinking model would otherwise spend on
reasoning before writing a word. A compaction that comes back empty reports
what the reply did instead — the reason the stream ended and the tokens it took
— and one that comes back cut is refused: the history stays as it was, and the
line says why.

## Sandbox

`bash` is confined by the kernel, not by string checks. On Linux that is
Landlock plus one seccomp rule, on macOS a Seatbelt profile. The confinement
applies in explore mode and is about writes and execution; normal mode keeps
only `no_new_privs` and the scrubbed environment.

A target with neither — anything but Linux and macOS — compiles, but no child
is ever spawned unconfined there: `bash` and MCP servers are refused in both
modes, and startup says so.

In explore mode, writes outside two directories are denied and only an
allowlist of programs may be executed: the common inspection and tooling set —
text and coreutils (`sed`, `awk`, `xargs`, `tee`, `nl`, …), checksums, file
operations (`cp`, `mv`, `rm`, `mkdir`), search (`grep`, `rg`, `fd`, `find`),
`git`, `curl`, `jq`, interpreters (`python3`, `node`, `perl`, `ruby`) and
package managers (`apt`, `npm`, `pip`, `cargo`, …) — plus the shell itself, plus
the ELF interpreter each of those binaries names, read from `PT_INTERP` rather
than assumed, since a NixOS system keeps it in `/nix/store`, plus git's network
transports (`git-remote-https` and its siblings), which `git` execs from its
own libexec directory rather than from `PATH`. A name that is not installed is
skipped, so the same list works on a thin system.

The list is a convenience guard, not a boundary: the kernel contains the side
effect. `npm install` and `apt install` are allowed to start and then fail on
their writes, and `curl -o` into the working directory fails for the same
reason. The writable paths tell the whole story: the scratch directory
(`$TMPDIR` inside a tool call, removed at exit) and the project's `plan/` under
the state tree, which survives so a plan can outlive the session. Neither is
executable — a script written there is a file, not a program. Three classes are
left off on purpose: daemons that write through another process (`docker`,
`systemctl`, `dbus-send`), setuid (`sudo`, `mount`), and programs whose side
effect is a mode or a timestamp rather than a write (`chmod`, `touch`) —
Landlock has no right for those. `nix` and `brew` are also off: they exec their
own `/nix/store` and `/opt/homebrew` paths, which no list entry can cover.
`ld.so` can start a program that is not on the list, and an allowed program can
load code with `LD_PRELOAD`; loaded code runs in the same domain, and still
cannot write outside the two directories. So `git ls-remote` reaches an https
remote in explore mode, while `git fetch` does not, because the repository is
not writable there, and an `ssh` remote does not, because the transport execs
`ssh`. It degrades with one line at startup on a kernel without Landlock, and
with one on a kernel whose Landlock is older than ABI 3 (Linux 6.2), where
truncation is not mediated at all; that line joins the session's history too, so
the model is told which rules the kernel is actually holding.

The network is not confined, in either mode: a command connects where you
connect, private ranges included. `io_uring_setup` is the only syscall denied,
in explore mode, because Landlock does not mediate io_uring and allowing it
would punch a hole in the write confinement. Normal mode installs no filter, so
a 32-bit binary runs there.

A refusal comes from the kernel, not from axe's argument parsing, and its
message says so: a missing binary and a denied one read differently, and a
denied write comes back with the two directories that would have been allowed
and the toggle that lifts both.

In explore mode an MCP server gets the same two writable directories and the
same `io_uring` rule; its exec is not confined, because a server starts its own
runtime (`npx`, `uvx`) that a leading-word check would refuse.

A command child inherits a minimal environment — `PATH`, `HOME`, `LANG`,
`LC_ALL`, `TERM`, `TMPDIR`, `USER`, `SHELL` — not axe's own, so provider and
search tokens stay in the parent's memory. A server gets those plus the
variables its entry names.

Known residuals: writes through an inherited file descriptor, truncation on a
kernel whose Landlock predates ABI 3, metadata such as `chmod` outside the
scratch directory, and `fetch`/`web_search`, which run in axe's own process by
design. Code loaded by an allowed program runs too, inside the same write
confinement.

## Trust and security

axe runs as you. The trust boundary is the operating-system user: the model can
reach the files, the environment and the credentials you can reach. A working
directory is not trusted just because it is yours — a cloned repository, its
`AGENTS.md`, a file, a fetched page or a command's output is data about the
task, not authority over it. Source labels record provenance; they are not
enforcement.

Normal mode runs code as you, with no approval prompt in between — a deliberate
trade-off: `bash` is a full shell, `write` and `edit` change files, and text in
a cloned repository or a paste can talk the model into running something. Use
it only in directories you trust. A project's `AGENTS.md` and its skills are
read at startup and go into the prompt ungated: where you start axe is the
whole decision. Its `.mcp.json` is the one exception — a server is code, not
text, so the file needs one approval per content hash (the host's `sha256sum`,
or `shasum` on macOS; a host with neither cannot approve one), and the prompt
shows every command and every variable it would hand over. `fetch` and
`web_search` are refused here, so a command is the only way to the network and
nothing stands between it and the internet.

Explore mode bounds integrity, not secrecy. The kernel denies writes outside
the scratch directory and the project's `plan/`, and direct execution outside
the allowlist. Reads are not confined: `read` and `bash` can read any file you
can, and `curl`, `fetch` and `web_search` can send any of it to any host.
`!cmd` is your own shell and bypasses explore mode's write confinement. When
confidentiality matters, use a container, a VM, or a dedicated user.

Prompt injection cannot be sanitized away. `fetch` and `web_search` results
start with `source: ... (untrusted)`, a project's instructions are wrapped in
`<project_instructions path="...">`, and the first rule says such content is
data. The web sanitizer removes terminal controls and chat-template tokens, not
persuasion: content that can address the model can still steer it. Reading the
transcript shows what happened; it is not a boundary either.

What the model reads enters the conversation and goes to the provider with the
next request. Treat the provider as inside the trust boundary, along with
anyone who can read the session files: plaintext JSONL, `0600`, unencrypted.
Provider and search tokens are read from the environment at startup and stay in
memory: children do not inherit them, `read` refuses `/proc` environment files,
and the config is rejected if it carries a literal key.
