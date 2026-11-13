# Rendering and streaming

Axe review prompt — one concern: **model, user and tool text reaches the
terminal as complete, safely styled blocks with no control bytes and no lost
content, and a streamed answer produces the same transcript as the same text
delivered at once.**

Read first: `AGENTS.md` invariants 5, 7, 13, 14 and the Conventions on
transcript output, markdown, style and colors; README "REPL" (the box, status,
glyphs and colors) and "Trust and security" (sanitizer). Then `src/markdown.rs`
in full, `src/render.rs`, `src/text.rs`, and `ui.rs` (`text`, `streamed`,
`output`, `thought`, `notes`, `error`, `insert`, `wrap`, the `Paint` use).

Verify with depth — render the hard cases, do not skim:

- Blocks: paragraphs, headings (all levels), tight and loose lists, nested and
  ordered lists with a start number, blockquotes, fenced code with and without
  an info string, an unclosed fence, indented code, horizontal rules, tables
  (alignment, escaped pipes, a wide cell, no header separator), links,
  autolinks, images, raw HTML blocks, HTML entities (named, decimal, hex,
  invalid), and hard breaks. Check a block that ends at EOF without a blank
  line and a document that is only a table or only a code fence.
- Inlines: emphasis, strong, strike, nested emphasis, code spans with one and
  two backticks, backslash escapes, bare URLs, `javascript:`/`data:` targets,
  mailto, and a URL whose visible text differs from its target. Check that an
  unsafe target is shown as text, not silently dropped, and that a link's
  target is not lost in the transcript.
- Streaming: `Stream` lays a block out when it ends and holds an unfinished
  line; check a fence split across reads, a table whose first row is partial, a
  paragraph followed by a list without a blank line, CRLF, a very long line, an
  empty stream, `[DONE]` in the middle of a block, and text after a tool call.
  The live row must say what the turn is doing, not style an unfinished line as
  final. Compare streamed output with the same string rendered at once; any
  difference is a finding.
- Width: 0 means unbounded; a fixed width wraps by cells, not chars. Check CJK,
  emoji with variation selectors, combining marks, tabs expanded as the
  terminal does, a code block wider than the terminal, table columns, and a
  link target that makes a line overflow. The prompt and the transcript must
  agree on what one cell is.
- Style: `Paint` is the only table; `Info` names a variant rather than defining
  one. Check every `Paint` variant is used and every use has a variant, that
  only named ANSI colors appear, that SGR is reset after each span, and that
  nested styles do not leak. Check markdown's code, quote, heading, link and
  dim styles against the theme's background.
- Control bytes and safety: strip ESC, CSI, OSC, C1 and other controls from
  model text, fetched pages, command output, file names, skill descriptions and
  tool arguments; keep `\n` and `\t` where layout needs them. Check a page that
  tries to set the title or clipboard, an invalid UTF-8 byte sequence, a NUL, a
  backspace, and a right-to-left override. `terminal_text` must strip only when
  stdout is a terminal; redirected output keeps the bytes.
- Caps: 50 000 chars for `read` and `fetch`, 30 000 for `bash` and an MCP
  result, 1 MiB off the wire for `fetch`, 64 MiB of captured command output.
  Check the boundary is exact, the cut cannot split a UTF-8 char, the truncation
  marker is present and says what was cut, and head-and-tail keeps the error at
  the end and the command at the start.
- Transcript lines: a tool call is one line whatever its arguments hold (name,
  command, mark, exit); consecutive calls are one block; a multi-line shell
  command and its output read under it; the reasoning line ends when the answer
  starts; an error and a notice carry their glyph and color. Check `/thoughts`
  off/on mid-stream, a tool result that is only whitespace, and a reply that is
  only reasoning.
- Output paths: the screen's cells and the piped ANSI-free text must carry the
  same characters in the same order. Check `text.rs`'s `terminal_text` in both
  modes, `render.rs`'s one-line forms, and `Screen::text`'s markdown collector
  against `Screen::output`'s line-by-line writer.

For each finding give the input (bytes or a short excerpt), `file:line`, the
invariant or README sentence it violates, and the visible symptom: content
lost, a table misaligned, an escape sequence reaching the terminal, a stream
that duplicates or drops a word, or a code block that wraps into prose.

Out of scope: the editor and key handling (see `prompt-editing.md`), frame
height and resize (see `frame-and-transcript.md`), and the HTTP/SSE layer that
delivers the text (see `http-and-sse.md`).
