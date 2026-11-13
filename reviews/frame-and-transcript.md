# Frame and transcript

Axe review prompt — one concern: **the owned region is exactly as tall as what
it shows, never touches the transcript or the bottom edge, and after any
sequence of writes, resizes, chooser entries and mode changes the cursor is
where the next block belongs.**

Read first: `AGENTS.md` invariants 12–14; README "REPL" (the box, the frame,
the status row, the chooser, the startup list) and "REPL" keys. Then `ui.rs`
(`Frame`, `Frame::layout`, `Shown`, `Screen::draw`, `bottom`, `settle`,
`write`, `flush`, `invalidate`, `repaint`, `clear_menu`, `reset`,
`prompt_layout`, `wrap`, `separated`), `repl.rs` (`readline`, `pump`,
`choose`, `show_queue`), and `main.rs` (`replay`, `new_session`, the sinks).

Verify with depth — build the row arithmetic for each state, do not skim:

- Height: the frame is a blank first row, then the candidate list or the
  chooser, a blank row, the queued lines and hint, a blank row, the prompt's
  wrapped rows, a blank row, the status/info row, and a blank row. Check each
  combination with zero rows available, an 80×24 terminal, a 1-row terminal, a
  prompt that wraps once and many times, a queue one line and taller than the
  screen, candidates above the prompt, and a chooser taller than the terminal.
  No row may be written against the bottom edge; the prompt may use as many
  rows as the terminal can spare and no more.
- Prompt layout: wrapping by display cells, not chars; wide CJK and emoji,
  combining marks, tabs, control bytes drawn as one cell, an empty line, a
  trailing newline, and a cursor at a wrap boundary. Check `prompt_layout`'s
  cursor mapping against what the terminal shows and where a typed character
  will land.
- The first row: it is blank whatever the frame draws, so nothing touches the
  block above. Check a full-screen chooser, a frame with no list, and a frame
  that changes height while the transcript above it is under the cursor.
- `slack`: after a frame shrinks, the blank rows it left above are counted and
  the next block fills them from the top. Walk a sequence: a 3-line queue
  shrinks to 0, then a tool call writes a block; a chooser closes without
  resuming and commits its rows; a resize arrives between two blocks. The next
  block must sit under the block before it, never under a gap, and no row may
  be lost or duplicated.
- Resize and reflow: a resize invalidates first, repaints the area under the
  cursor even when no state changed, settles the frame back on the bottom, and
  drops `slack` because no row above is known blank. Check a resize during a
  streaming turn, during a chooser, while the prompt is taller than the new
  screen, and when the terminal reports its old size for one event. Check the
  `@resize:COLSxROWS` harness path.
- One writer: only `Screen::write` and `Screen::settle` write rows above the
  frame; every setter and transcript writer marks dirty; `repaint` paints only
  when dirty. Check a state change that forgets to mark dirty, a repaint that
  redraws everything anyway, and a work item finishing off the UI thread that
  strands a row. No stray `print!` while a `Screen` exists; the startup banner
  and piped output are the only writers before/without it.
- Blocks: `Block` and `separated` are the one rule for the blank row between
  two blocks; a block never begins or ends with a blank row; consecutive tool
  calls are one block. Check a block ending in a code fence, a table, a blank
  markdown paragraph, a tool call with no result, and a notice after a turn.
- The transcript: streamed text lands once, a thinking line ends when the
  answer starts, tool calls and their results order correctly, errors and
  notices use their own glyphs, and a resumed session replays the same blocks
  without the system prompt or mode notes. Check `/new` wiping the screen and
  scrollback, and `/compact` printing its summary once under the notice.
- Choosers: rows are committed to scrollback when the chooser closes without
  resuming; startup's chooser vanishes on the first typed key; `/sessions`
  filters; `/model` and `/thinking` narrow and wrap. Check the selected row
  after filtering, an empty list, Esc, Enter on an empty filter, and a list
  that shrinks below the selection.
- Piped mode: no splash, prompt, frame or notice; stdout is the model's text
  (and what a line explicitly asks for), harness lines go to stderr. Check a
  turn that calls a tool, a `!cmd`, `/sessions` and `/model` without a screen.

For each finding give the terminal size, the state sequence, `file:line`, and
the visible wrong frame: rows drawn over the transcript, blank rows left above
a block, a prompt row below the bottom edge, a lost line, or a cursor one cell
off from where the next character appears.

Out of scope: key semantics and the editor (see `prompt-editing.md`), the
content and styling of markdown (see `rendering.md`), and thread interactions
(see `concurrency.md`).
