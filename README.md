# axe

```text
 ▄▄▄  ▄▄ ▄▄ ▄▄▄▄▄
██▀██ ▀█▄█▀ ██▄▄
██▀██ ██ ██ ██▄▄▄
```

A minimal coding agent in Rust. One binary, one line REPL, any
OpenAI-compatible `chat/completions` server, seven tools, **six dependencies**. The transcript is
your scrollback: axe never takes the screen.

Axe starts in read-only mode by default; web search works there only.
Axe does not re-implement or compete with the functionality of your other tools, shell, or Unix system.

## Install

```sh
curl -fsSL https://raw.githubusercontent.com/qxphq/axe/main/install.sh | sh
```

That fetches a build of `main` from the rolling `latest` release, checks its
sha256 and installs the binary into `~/.local/bin` (`AXE_INSTALL_DIR` overrides
it). Assets exist for x86_64 and aarch64 Linux (glibc; on musl, including
Alpine, the script says so and stops) and for Apple Silicon and Intel macOS.
The release is rebuilt on every push to `main`, so its version is whatever
`Cargo.toml` says at that commit — no tag marks it.

## Build

```sh
cargo build --release
```

The binary is `target/release/axe`. It needs a current stable Rust toolchain
(edition 2021) and a C toolchain for `ring`. Linux (x86_64 or aarch64) and macOS
are supported. Other Unix-like targets compile, but `bash` is unavailable
there: a child that cannot be confined is not spawned. Windows is not supported.

`cargo install --git https://github.com/qxphq/axe --locked` builds the same
binary from source, without the release pipeline.

## Run

```sh
axe              # in the directory you want it to work on
axe < prompts    # without a terminal: plain-text lines in, transcript out
axe --version    # prints the version and exits; no config, no network
```

Without a terminal the line comes from stdin and the transcript goes to stdout:
the same commands, the same turns, one line at a time. Nothing that needs a
screen exists there — `/sessions` prints a numbered list to resume from, and the
startup chooser does not appear.

The working directory is what the prompt shows, what `AGENTS.md` is looked up
from, and the key under which the session is remembered.

The version follows semver and is written in exactly one place, `Cargo.toml`:
the banner and `--version` read it at compile time, and `flake.nix` reads the
same file. The `ship` skill bumps it with the change.

## Config

`~/.config/axe/config.json`, or `$XDG_CONFIG_HOME/axe/config.json`. It is
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
| `skills` | extra skill directories or `SKILL.md` files |

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

Thinking levels are named by the provider, not by axe: `off`, `low`, `medium`
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

The built-in prompt is the working directory and a short numbered list of
rules; tool names, descriptions and schemas are sent only in the request's
`tools` array, never repeated in the prompt. Instructions are appended after
it, in this order: `agents` from the config, then the nearest `AGENTS.md` at or
above the working directory, then `system`. `agents` and `AGENTS.md` are capped
at 48 KB; `system` is not. All are read once at startup; nothing is re-read
mid-session, and the mode is carried in the conversation, not the prompt. The
skills catalogue, when there is one, is appended last.

A project's `AGENTS.md` comes from the repository, not from you, so it is
wrapped in `<project_instructions path="...">`. The first rule in the prompt
says that block is data: it describes the project, and it never overrides your
current request.

## Skills

A skill is a directory containing `SKILL.md`: YAML frontmatter (`name`,
`description`) followed by instructions. axe implements the Agent Skills
format (`agentskills.io`); relative references in the body resolve against the
skill's directory. Only the name and description are loaded at startup — the
body is read when the skill is used.

Locations, highest precedence first:

| scope | path |
| --- | --- |
| project | `.agents/skills/` in the working directory and each ancestor up to the repository root |
| global | `<config dir>/skills/`, then `~/.agents/skills/` |
| config | each `skills` path; `~/` expands, relative to the config |

Directories are scanned recursively; a directory holding `SKILL.md` is a skill
and is not descended into. The first name wins, so a project skill replaces a
global one with the same name. `.git`, `node_modules`, `target` and dot
directories are skipped; at most 200 skills and 16 KB of catalogue are used.

Project skills come from the repository and load like any other; the startup
notice says how many joined the prompt. There is no trust prompt and no gate:
loading is what `AGENTS.md` and a skill are for, and a directory you would not
trust is a directory you should not run a coding agent in.
`/skills` lists what was found and where it came from.
`/skill:name request` loads a skill explicitly and runs a turn; `/skill:name`
completes with Tab. `allowed-tools` is parsed and shown but not enforced — axe
has no approval layer to pre-approve against. `disable-model-invocation: true`
keeps a skill out of the model's catalogue while `/skill:name` still works.

The catalogue is a section of the system prompt listing each visible skill's
name, description and absolute `SKILL.md` path; the model reads the file when
a task matches.

## REPL

The line you type sits in a box: an outline in the prompt's cyan, one row per
line the text wraps to, with a cell of padding between the text and the line and
no glyph in front of it. While explore mode takes the writing tools away the
outline is yellow, so the read-only tool set shows at the line you are typing
and not only in the info row beside it. The outline is the only thing drawn —
the text keeps the theme's own foreground and background — because a filled
block that wide reads as a slab of color rather than as a prompt. Enter submits;
Ctrl+J or Alt+Enter starts a new line. Typing a slash word lists the matching
commands in
the rows above the prompt — as many as the terminal's height allows, up to four
— the first one marked. ↓ and ↑ move the mark and leave the line as typed,
wrapping at both ends; Enter runs the marked row and Tab puts it on the line, so
a step never rewrites the prompt by itself. A command's first argument completes
the same way: `/model ` lists the models, `/model dee` narrows them, and Enter
applies the marked one. The rule does not depend on where the list came from: a
path in a message lists what it could be, and Enter submits the completed line.
A model or a thinking level is therefore chosen in the prompt, like any other
line. A line starting with `!` is your own shell: it runs through the same tool,
its output joins the history as a user message, and no turn follows — the model
sees it the next time you ask something.

Every submitted line is remembered for the directory, across sessions, so ↑
walks what was typed here before, newest first; ↓ comes back down and gives the
line being typed back at the bottom. Ctrl+R searches instead: the query shows
on the row under the prompt, the line previews the match, and Ctrl+R again steps
to an older one. Enter accepts the match into the line without submitting it,
and Esc puts back what was there before. A blank line, a line starting with a
space (the shell's "do not remember this") and a repeat of the line before it
are not remembered.

Tab completes paths as well. A token that starts with `@`, `~`, `/` or `.`, or
holds a `/`, lists the directory it names — relative to the working directory,
absolute from `/`, `~` from home — with directories first and a trailing `/` on
each, so Tab on a directory keeps the list open and a lone file ends it with a
space. `@` is a trigger, not part of the path: `@READ` completes to
`README.md`, and what the model reads is the path, not a sigil. A word that is
none of those is prose, and Tab leaves it alone: a file in the working directory
is reached with `@`, and a root path with `@` or its own slash, because `/model`
is a command. A path with a space in it is not completed. Enter runs the
marked entry here as it does for a command, so a message about a file submits
the file.

| command | what it does |
| --- | --- |
| `/model [id]` | list the models, or switch to one (Tab completes it); without a terminal it only lists |
| `/thinking [level]` | show or set the thinking level |
| `/thoughts` | show or hide the model's reasoning |
| `/explore` | toggle explore mode (also Shift+Tab) |
| `/new` | start a new session, naming the old one and clearing the screen |
| `/sessions [query]` | browse or search this directory's sessions |
| `/compact` | summarize the session and continue from the summary |
| `/skills` | list loaded skills, where they came from, and what the model sees |
| `/skill:name [request]` | load a skill's instructions and run a turn |
| `/help` | list the commands, the config path and its keys |
| `/exit`, `/quit` | quit |

The area under the transcript is as tall as it has to be: a blank row above the
prompt, the prompt's rows, a blank row, the status, and a blank row under it,
with the queued line and the candidates on top of that. The blank rows around
the prompt are the frame's own, so the prompt keeps one above it and one below
it whatever is shown. The status says what the turn is doing — thinking, running
a command, responding — and is blank only when nothing is in flight, so a wait
on the provider is never a silent row. A chooser is the one part sized by the
terminal — a window over the transcript, whose rows are committed when it
closes.

Recent sessions for the directory are offered at startup as a chooser: Up and
Down move, Enter resumes, Esc closes it, and typing starts a new session and
takes the list with it. `/sessions` opens the same list, where typing filters it
and Enter on an empty line resumes the selection.

| key | what it does |
| --- | --- |
| Enter | submit the marked candidate when a list is under the prompt, else the line |
| Ctrl+J, Alt+Enter | new line |
| Tab | complete a command, its first argument, or a path token: put the marked candidate on the line without submitting it |
| ↑ ↓ | move the mark over the candidates above the prompt, walk the history when there is no list, or move in a chooser |
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
a line typed meanwhile is queued. It joins the turn at the next round boundary —
after the tool calls the last answer asked for have run, before the next request
— so the model reads it while it is still working rather than after it is done.
A command (`/model`, `/sessions`, `!cmd`) needs the prompt and the session, so it
waits for the turn to end. Interrupting takes two presses: the first puts
the offer on the row under the prompt, any other key withdraws it, and the
second stops the turn — which then says so in the transcript, because a cut
reply reads like a finished one.

The row under the prompt is what the turn is doing on the left, and on the right
the session's name, the provider/model and thinking level, and how much of the
context window the history fills. Each piece on the right carries its own color,
and every color is one of the terminal's own ANSI names, so the theme decides
what it actually is: the name in the default foreground, the provider/model in
the prompt's cyan, the thinking level in magenta, the window green until it is
70% full, yellow to 90%, then red, and `read-only` in yellow. The right half
never takes more than half the row: a long title clips, it does not push out
what the turn is doing. The name is the first request until the
model names the session after the first answer; it appears as soon as the
request is in the history, and the generated title replaces it without waiting
for a key.

## Modes

Explore mode is a session setting, on by default (`explore_by_default`). When
it is on: `write` and `edit` are unavailable, `bash` runs only a fixed
allowlist of commands and writes only inside a scratch directory and the
project's `plan/` directory,
and `fetch` and `web_search` are the way to read the web. When it is off,
`write`, `edit` and a full `bash` are back — `fetch` and `web_search` are
refused. The network is confined in neither mode: a command connects where you
connect. Every change is announced
in the conversation as `explore mode: on` or `explore mode: off`, so the model
knows which side it is on.

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

`read` returns lines without numbering. `bash` starts every call in the working
directory, so `cd` is pointless; output is truncated, not streamed.
`fetch` takes 1 MiB off the wire at most and gives the model at most 50 000
characters, with `start` and `chars` to page through a longer page.
`web_search` asks Brave when a token resolves and DuckDuckGo otherwise, falling
back if Brave fails or returns nothing; `count` defaults to 10 and caps at 20.

## State and sessions

Per directory, under `$XDG_STATE_HOME/axe/projects/` (or
`~/.local/state/axe/projects/`), in a subdirectory named after the working
directory with every byte outside `[A-Za-z0-9._-]` written as `%XX` — so
`/home/anon/axe` becomes `%2Fhome%2Fanon%2Faxe`.

| file | contents |
| --- | --- |
| `state.json` | model, thinking levels, thoughts toggle |
| `history.jsonl` | prompt lines typed here, one JSON string per line; the newest 500 |
| `plan/` | explore mode's writable directory: a plan that outlives the session |
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

`/compact` replaces the history with a summary of it. The body is not rewritten:
the summary is appended as a marked message, and reading a body starts from the
last marker, so replay matches what the live session sent. What came before the
marker stays on disk and `/sessions` search still finds it. The summary keeps
the system prompt and the current mode note; a session that has been compacted
lists the messages the model still has, not the bytes in the file.

## Sandbox

`bash` is confined by the kernel, not by string checks. On Linux that is
Landlock plus one seccomp rule, on macOS a Seatbelt profile. The confinement
applies in explore mode and is about writes and execution; the network is not
confined, and normal mode keeps only `no_new_privs` and the scrubbed
environment.

In explore mode, writes outside two directories are denied, and only
an allowlist of programs may be executed: the common inspection and tooling set —
text and coreutils (`sed`, `awk`, `xargs`, `tee`, `nl`, …), checksums, file
operations (`cp`, `mv`, `rm`, `mkdir`), search (`grep`, `rg`, `fd`, `find`),
`git`, `curl`, `jq`, interpreters (`python3`, `node`, `perl`, `ruby`) and package
managers (`apt`, `npm`, `pip`, `cargo`, …) — plus the shell itself, plus the ELF
interpreter each of those binaries names — the kernel opens it for every
dynamically linked binary, and it is read from `PT_INTERP` rather than assumed,
since a NixOS system keeps it in `/nix/store` — plus git's network transports
(`git-remote-https` and its siblings), which `git` execs from its own libexec
directory rather than from `PATH`. A name that is not installed is skipped, so
the same list works on a thin system.

The list is a convenience guard, not a boundary. The kernel is what contains a
side effect: `npm install` and `apt install` are allowed to start and then fail
on their writes, and `curl -o` into the working directory fails for the same
reason. The writable paths tell the whole story: the scratch directory
(`$TMPDIR` inside a tool call, removed at exit) and the project's `plan/` under
the state tree, which survives so a plan can outlive the session. Neither is
executable — a script written there is a file, not a program. Three classes are
left off on purpose: daemons that write through
another process (`docker`, `systemctl`, `dbus-send`), setuid (`sudo`, `mount`),
and programs whose side effect is a mode or a timestamp rather than a write
(`chmod`, `touch`) — Landlock has no right for those. `nix` and `brew` are also
off: they exec their own `/nix/store` and `/opt/homebrew` paths, which no list
entry can cover. `ld.so` can start a program that is not on the list, and an
allowed program can load code with `LD_PRELOAD`; loaded code runs in the same
domain, and still cannot write outside the two writable directories. So `git ls-remote`
reaches an https remote in explore mode, while `git fetch` does not, because the
repository is not writable there, and an `ssh` remote does not, because the
transport execs `ssh`. It degrades with one line at startup on a kernel without
Landlock.

The network is not confined, in either mode: a command connects where you
connect, private ranges included. `io_uring_setup` is the only syscall denied,
in explore mode, because Landlock does not mediate io_uring and allowing it
would punch a hole in the write confinement. Normal mode installs no filter, so
a 32-bit binary runs there.

A refusal comes from the kernel, not from axe's argument parsing, and its
message says so: a missing binary and a denied one read differently, and a
denied write comes back with the two directories that would have been allowed
and the toggle that lifts both.

A command child inherits a minimal environment — `PATH`, `HOME`, `LANG`,
`LC_ALL`, `TERM`, `TMPDIR`, `USER`, `SHELL` — not axe's own. Provider and
search tokens stay in the parent's memory.

Known residuals: writes through an inherited file descriptor, metadata such as
`chmod` outside the scratch directory, and `fetch`/`web_search`, which run in
axe's own process by design. In explore mode, code loaded by an allowed program
(`LD_PRELOAD`, the loader) runs too, inside the same write confinement.

## Trust and security

axe runs as you. The trust boundary is the operating-system user: the model
can reach the files, the environment and the credentials you can reach. A
working directory is not trusted just because it is yours — a cloned
repository, its `AGENTS.md`, a file, a fetched page or a command's output is
data about the task, not authority over it. Source labels record provenance;
they are not enforcement.

Normal mode runs code as you. `bash` is a full shell, `write` and `edit` change
files, and no approval prompt stands in between — a deliberate trade-off. A
cloned repository's instructions, a file, or text you paste in can talk the
model into running something. Use normal mode only in directories you trust.
The project's `AGENTS.md` and its skills are read at startup and go into the
prompt; axe asks nothing and gates nothing, so the directory you start it in is
the whole decision.
`bash` runs as you and reaches the network as you do, and `fetch` and
`web_search` are refused here, so in normal mode a command is the only way to
the network and nothing stands between it and the internet.

Explore mode bounds integrity, not secrecy. The kernel denies writes outside
the session scratch directory and direct execution outside the allowlist; on a
kernel without Landlock, the startup line says when that is weaker. Reads are
not confined: `read` and `bash` can read any file you can, and `curl`, `fetch`
and `web_search` can send any of it to any host. `!cmd` is your own shell — it
bypasses explore mode's write confinement. When confidentiality matters, use a
container, a VM, or a dedicated user.

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
