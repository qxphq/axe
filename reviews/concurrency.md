# Concurrency and cancellation

Axe review prompt — one concern: **exactly one thread mutates the session and
one owns the terminal, and every cross-thread path either queues work or drops
it deliberately — never races, deadlocks, or paints from the wrong thread.**

Read first: `AGENTS.md` invariants 12–13 and the mid-turn queue paragraph in
Conventions; README "REPL" (turns in the background, queued lines, two-press
interrupt) and "Modes". Then `main.rs` (`Event`, the two producers, `event`,
`key`, `send`, `run_interactive`, `run_piped`, the turn spawn), `repl.rs`
(`spawn_input`, the cursor-report filter, `pump`, `readline`, `choose`),
`turn.rs` (`run_turn`, `take_queue`, the sinks), `tools.rs` (the bash child and
cancel), `http.rs` (the response thread and `Cancel`), and `mcp.rs` (the stdio
reader, the HTTP connection, `restart`).

Verify with depth — follow each event from producer to mutation:

- Topology: list every thread the process starts, what it owns, what it sends,
  and what it blocks on. Check the main thread never performs a blocking
  provider call, the input thread never touches the session, and the turn
  thread never draws. Name every `recv_timeout`/`poll` interval and what wakes
  it; an idle tick that does work every 100 ms is a finding if the work is not
  needed.
- Ordering: keys, turn events, naming results and MCP call results share one
  channel. Check that a batch is applied in arrival order, that the screen is
  repainted once per drained batch, that the status does not outlive its turn,
  and that a resize or paste arriving between events is not lost. Check what
  happens when the receiver is gone (send errors are ignored?) and when the
  channel disconnects mid-turn.
- Screen ownership: only the main thread calls `Screen` methods. Check the
  `Sink` implementations, the naming thread, the MCP reader, and the bash
  child's output path. A stray write to stdout while the screen exists is a
  rendering bug; a `Screen` method reached from another thread is a data race
  in spirit even when it compiles.
- Cancellation: first Esc offers, any other key withdraws, second stops. The
  flag must reach the provider socket, the bash child (`kill`/process group),
  an MCP stdio call (server kept) and an MCP HTTP request (socket closed).
  Check the race where the turn finishes between the two presses, a cancel
  while queued lines are waiting, and a cancel during naming or compaction.
  The turn must end with one status, one notice, and no thread left running.
- The queue: `take_queue` runs only at a round boundary, after the tool results
  and before the next request. Check a line submitted while the boundary is
  being read, the order of several lines, a command that arrives in the same
  batch, and `↑` taking lines back concurrently. A line already appended to the
  history must not be recallable; one still queued must not be lost.
- Naming and compaction: both spawn work and poll a receiver. Check a session
  switch, `/new`, or a quit while they run; a result applied to the session it
  was started for; a second naming; and a poll that only runs on the idle tick
  (what if the user types continuously?).
- MCP: the stdio reader thread parses one JSON-RPC reply at a time; check
  request-id matching, a server that prints something that is not JSON, stderr
  tailing, timeout, server exit, a call cancelled while the process stays up,
  and `/explore` restarting servers while a call is in flight. Check the HTTP
  path's session id shared across calls and its DELETE on close.
- Locking: axe has few mutexes (`mcp::lock`, the stderr tail, statics). For
  each, name the critical section, what can block inside it, and whether a
  second thread can re-enter. Check `OnceLock` initialization under a race
  (two threads touching `definitions()` or the sandbox scratch) and any
  `unsafe` shared state.
- Pipes and children: stdout/stderr of a bash child are read while it runs;
  check a child that fills stderr, a child that ignores SIGTERM, a child that
  exits before its pipes are read, and the temp output files. Check MCP's
  `read_stderr` cannot fill its tail buffer forever.

For each finding give the interleaving (which thread does what, in which
order), `file:line`, the invariant or documented behavior it breaks, and the
visible symptom: a hung prompt, a lost line, a double-submitted turn, a stale
status, a corrupted history, or a process that will not exit.

Out of scope: frame geometry and painting rules (see
`frame-and-transcript.md`), HTTP framing and SSE (see `http-and-sse.md`), and
MCP config/trust semantics (see `config-and-mcp.md`).
