# Performance and snappiness

Axe review prompt — one concern: **startup reaches the prompt quickly, a
keystroke and a streamed chunk cost only what that interaction needs, and
nothing scales with transcript length or file size without a reason.**

Read first: `AGENTS.md` invariants 12–13 and the note that a repaint per token
is waste; README "REPL" (turns in the background, the frame and status); then
`src/ui.rs` (`draw`, `paint_above`, `settle`, `wrap`, `Screen::write`,
`repaint`, `invalidate`), `src/repl.rs` (the input thread, `readline`, `pump`,
the 100 ms/50 ms waits), `src/markdown.rs` (`Stream`, block layout), `src/main.rs`
(the event loop, `replay`, `compact`, `compaction_prompt`, `history_bytes`),
`src/sessions.rs` (`list`, `load_messages`, `search`, `archive_old`),
`src/startup.rs` (config, models, skills, MCP, splash), and `src/http.rs`.

Measure, do not only read. Use `mock.py` and `harness.py` for the screen and
`!cmd`-style runs for anything the sandbox refuses; label an estimate when no
live run is possible.

Verify with depth — time the journeys a user repeats:

- Cold start: config load, provider model-list requests (in parallel or one
  after another?), splash output, skills discovery (how many directories, how
  deep), MCP start (parallel? one timeout each?), `AGENTS.md` read, session
  list, and the first prompt. Measure with a mock provider and with no network;
  identify what blocks the prompt and what only blocks the splash.
- Key to paint: a character, Backspace, an arrow over a 50-line history, Tab in
  a 10 000-entry directory, Ctrl+R, and a paste. Check whether the whole frame
  is repainted per key, whether the prompt is re-wrapped per key, and whether
  completion scans the filesystem when the key is not Tab. Measure in an 80×24
  and a 200×50 terminal.
- Idle: what runs every 100 ms and every 50 ms, whether an idle axe burns CPU
  or wakes the provider/MCP, and whether a status or naming poll draws when
  nothing changed. Check a waiting turn, a disconnected input thread, and a
  suspended terminal.
- Streaming: one channel event per delta, markdown line buffering, block
  layout, and one repaint per drained batch. Measure a 100 KB answer, a
  reasoning stream, a tool-call argument stream, and a `split:` reply (one
  chunk per character). Check for an O(n²) re-wrap, a per-chunk allocation, or
  a full transcript redraw.
- Transcript size: 10 000 lines of scrollback, a 1 MB paste, a 50 000-char tool
  result, and a code block wider than the terminal. Check whether a new block
  rewrites what is above it, whether `slack` placement scans the transcript,
  and whether the screen keeps every line in memory.
- Session operations: resume a 10 MB body, list 100 sessions, search 100
  sidecars and a gzipped body, write one message, and rewrite the 500-line
  history. Check whether `list` parses whole bodies, whether search reads more
  than it shows, whether gzip runs per session, and whether `history_bytes`
  re-serializes per keystroke.
- Compaction and naming: `compaction_prompt` removes the first element in a
  loop (O(n²)); measure a full 200 000-character budget. Check the summary
  request's timeout, the naming thread, and `/new`.
- HTTP and tools: the request body is built once, the response is streamed line
  by line, the 1 MiB line cap and 1 MiB body cap hold; `read` loads 1 MiB and
  returns 50 000 chars; `bash` captures up to 64 MiB; an MCP result up to 4 MiB.
  Check the truncation and control-strip passes for a second full copy.
- Paint mechanics: `repaint` paints only dirty cells; a resize invalidates and
  settles; the frame is as tall as it shows. Check the cell buffer diff on a
  300-row terminal, a cursor move, and a chooser opening/closing.
- Reference comparison, only where it is cheap: opencode/pi's event batching,
  context measurement, and tool-output handling are already covered by
  `token-efficiency.md`; borrow a loop shape only if axe has the measured
  symptom.

For each finding give the journey, the measured or labelled wall time/CPU/
memory, `file:line`, the operation that scales, and the user-visible symptom:
a prompt that appears late, typing lag, a stutter while a reply streams, a
pause on resume, or CPU while idle. Do not propose a micro-optimization without
a measurement; do not trade a documented invariant (one writer, append-only,
caps) for speed without saying so.

Out of scope: token cost and context economics (see `token-efficiency.md`),
frame correctness and layout (see `frame-and-transcript.md`), concurrency
correctness (see `concurrency.md`), and any optimization whose only evidence is
a code smell.
