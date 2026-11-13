# Prompt editing and completion

Axe review prompt — one concern: **every key does what the README says in every
state, and the line the model receives is exactly the line the screen shows.**

Read first: README "REPL" in full — the box, the candidate rules, the command
table, the key table, completion, history search, and the mid-turn queue.
`editor.rs` in full, `complete.rs` in full, `ui.rs` (`apply_key`, `Action`,
the menu and chooser methods, `commit`, `echo_user`), `repl.rs` (`readline`,
`pump`, `narrow`, `choose`, `take_back`, `recall_lines`), and `state.rs`
(`read_history`, `append_history`).

Verify with depth — press every key in context, do not skim:

- Buffer and cursor: UTF-8 char cursor, combining marks and wide chars,
  insertion at the cursor, backspace/delete at the edges, Ctrl+A/E, Ctrl+B/F,
  Alt+B/F word motion, empty buffer, one-character buffer, and a buffer ending
  in newline. Check that the cursor never splits a char and that the line shown
  is the line returned.
- Kill ring: Ctrl+W, Ctrl+U, Ctrl+K, Alt+D, Alt+W kill what the docs say; Ctrl+Y
  yanks the last kill; Ctrl+T and Alt+T transpose characters/words. Check
  killing twice (does the ring replace or append?), kill+move+yank, yank with
  an empty ring, and a kill at the start/end of the buffer.
- Editing and submission: Enter submits, Ctrl+J/Alt+Enter inserts a newline,
  Ctrl+C clears, Ctrl+D ends on an empty line and what it does on a non-empty
  one, paste with newlines and tabs, and a line starting with a space. Check
  that submission does not trim content the model should see, and that a blank
  line and a repeat are not added to the prompt history.
- History: ↑/↓ walk the newest 500 for the directory, newest first, at the
  first/last line of a multiline buffer; the bottom returns the line being
  typed; Ctrl+R searches, previews, steps older, accepts with Enter without
  submitting, and Esc restores. Check a query with no match, a query matching
  only an older line, two Ctrl+R presses, a history write failure, and a
  multiline history entry.
- Candidates: a slash word lists commands; a path token lists directories first
  with a trailing `/`; `@` is consumed; dotfiles wait to be asked for; a token
  with a space is not completed; `~`, `/`, `.` and a token with `/` complete
  against the right root. Check Enter taking the marked candidate and Tab
  putting it on the line, the mark wrapping at both ends, ↓/↑ leaving the line
  as typed, and a single candidate. Check `/model` and `/thinking` argument
  completion is deliberately off.
- Choosers: bare `/model`, `/model -g`, `/thinking`, `/sessions` and the startup
  list. Typing narrows by spec/id, not by displayed row; arrows wrap; Enter
  applies; Esc closes; a list that starts above the prompt; an empty filter;
  and a session switch while the list is open. Check `/model dee` opening
  narrowed and `/model provider/id` setting directly.
- Queue recall: ↑ with lines waiting takes them all back onto the prompt, above
  what was typed, in order, and empties the queue. Check ↑ when the queue holds
  a command, ↑ with a multiline typed line, and ↑ after the turn has read the
  lines (they must stay in the transcript and not come back).
- Explore and mode keys: Shift+Tab toggles; the outline turns yellow; `/new`
  resets explore on; a tool that disappears is not still in the completion or
  command list. Check that a key pressed during a turn is queued or applied as
  the docs say, not dropped.
- The echo: a committed line carries `█ ` in the transcript, not the box; a
  multiline line echoes all its rows; a wrapped box is one block; the cursor
  after submission sits where the next block begins. Check a paste of a whole
  file and a line wider than the terminal.

For each finding give the screen state, the key sequence, `file:line`, and the
wrong result: a cursor one char off, a line submitted different from the one
shown, a candidate that cannot be reached, a history step that loses the typed
line, or a key that does nothing where the README promises an action.

Out of scope: frame geometry and resize (see `frame-and-transcript.md`),
completion's filesystem errors as such (report only user-visible candidates),
and model/session semantics behind the choosers (see
`sessions-and-state.md`).
