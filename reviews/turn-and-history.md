# Turn loop and history

Axe review prompt — one concern: **one user line becomes exactly the rounds the
model asked for, and the messages in a request are the messages the session
file holds, in the same order, including after a compaction or a resume.**

Read first: `AGENTS.md` invariants 6 and 13, and the paragraph "A line typed
during a turn joins it at the next round boundary" in Conventions; README
"State and sessions" (the `/compact` paragraph), "REPL" (turns run in the
background), "Run" (piped output). Then `src/turn.rs` in full, `src/store.rs`,
`sessions.rs` (`load_messages`, `compacted`, `fallback`), `api.rs` (`Reply`,
`Assembler`), and `main.rs` (`messages_turn`, `run_turn`, `compact`,
`naming_excerpt`, the two sinks).

Verify with depth — trace actual message sequences, do not skim:

- A round: one request, the assistant message it produced, and the tool results
  it asked for before the next request. Check a reply with zero, one, several,
  and duplicate-index tool calls; a call with no id or an empty name; an unknown
  tool; a tool that answers an error; an empty content with a finish reason; a
  stream that ends without `[DONE]`. `unanswered` must leave the history
  request-valid even when the model never answers a call.
- Termination: the loop stops only on a reply with no calls. Check the round
  limit or its absence, a repeated identical call, a tool that always fails,
  and what the user sees when the turn is cancelled between rounds. A cut reply
  must not read as a finished one.
- The two sinks: the screen sink and `PlainSink` must receive the same text,
  reasoning, calls and errors. Check that reasoning is shown only when
  `/thoughts` is on, that streamed text and a finished block do not duplicate
  or drop a line, and that piped mode puts only the model's text on stdout.
- `Store::record` and the live history's `push`: every message appended to the request
  is appended to the JSONL once, in order, including the user line, assistant
  text and reasoning, tool calls and results, mode notes and `!cmd` output.
  Check the failure path: a body write that fails warns once and the session
  continues; a resumed body replays the same order; the system prompt and mode
  notes are never logged.
- The mid-turn queue: a line typed during a slow call joins after the tool
  results and before the next request; several lines keep their order; a
  command waits for the turn to end; `↑` takes back only what the turn has not
  read, above what is typed, and empties the queue. Check the same at the last
  round, when there is no next request.
- Compaction: `/compact` appends the marked summary, the live request drops
  everything before the marker, and a later resume starts from it with the
  marker key stripped. Check the budget arithmetic in `compaction_prompt`
  (characters, 200 000 cap, oldest lines dropped, per-role head/tail), a second
  compaction, a summary that comes back empty (`no_text`), and a summary whose
  own request would overflow. The summary is printed once, under its notice,
  and not again on resume.
- Naming: the request runs once after the first answer, with no tools and no
  output cap; the excerpt skips mode and shell lines; a failed or partial reply
  falls back to the first request; the title is applied at the next poll and
  never renames a session the user has left.
- Accounting: `used` comes from the provider's usage chunk and the info row
  falls back to bytes/4 only before the first answer. Check the reset on
  `/compact`, `/new`, resume, and a model switch; check that a round without a
  usage chunk does not leave a stale count.
- Errors: a provider error mid-stream, a refused request, a tool panic or a
  missing tool, and a cancelled compaction must each produce one result the
  model or the user can act on — never a partially appended assistant message
  that the next request rejects.

For every check, state whether it holds everywhere or where it breaks. For each
finding give the round and the exact message sequence, `file:line` on both the
producing and the consuming side, and the concrete wrong outcome (a 400 on the
next request, a duplicated tool result, a resumed history that differs from the
live one).

Out of scope: session file layout, archive and permissions (see
`sessions-and-state.md`), HTTP/SSE framing (see `http-and-sse.md`), frame
layout (see `frame-and-transcript.md`), and token cost of the resulting history
(see `token-efficiency.md`).
