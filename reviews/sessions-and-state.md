# Sessions and per-directory state

Axe review prompt — one concern: **every byte a directory remembers is written
once, privately, atomically, and a crash, a gzip-less host, or a resume can
never make the session list and the session body disagree.**

Read first: `AGENTS.md` invariants 6 and 10, and the Conventions paragraph on
sessions; README "State and sessions" (both tables and the paragraphs on
archiving, search and defaults), "Config", "Run". Then `src/state.rs` in full,
`src/sessions.rs` in full, `src/store.rs` (`fresh`, `resumed`, `record`,
`finalize`, `compacted`), `main.rs` (`resume`, `replay`, `new_session`,
`start_naming`, `poll_naming`, `name_session`, `sessions`), and `startup.rs`
(`run`, the default-model resolution).

Verify with depth — exercise the paths on paper with real inputs:

- The cwd key: every byte outside `[A-Za-z0-9._-]` becomes `%XX`; check case,
  unicode, `/`, a trailing slash, the filesystem root, a very long path, and
  two different paths that could escape or collide. Check whether the key is
  the raw `current_dir()` or a canonical path, and what a symlinked cwd does.
- Privacy: project directories `0700`, files `0600`, `private_dir`,
  `create_private` and `write_private`. Check a pre-existing directory or file
  with looser modes, a symlink planted at a state path, and a concurrent axe in
  the same directory. The atomic write must be temp-and-rename in the same
  directory, and a failed rename must not truncate the old file.
- The sidecar: fields and `VERSION`, the session id (32 hex, `valid_id`),
  title, summary, count, dates, model and thinking if stored. Check list order,
  a sidecar without a body, a body without a sidecar, a truncated/corrupt JSON
  sidecar, a zero count, and an id supplied on the command line or by
  `/sessions` (traversal, uppercase, non-hex).
- The append-only body: one JSON message per line, newline before append,
  no rewrite except the compaction marker, a crash leaving an unfinished tail
  line. Check whether the loader skips only the bad tail or silently accepts a
  corrupt middle line, and whether a write failure mid-session leaves the
  request and the file diverged.
- Archive: `KEEP` 30, gzip through the host's `gzip`, `.jsonl.gz` naming, meta
  kept plain, compress-unlink order, an interrupted compress, a host without
  `gzip`, and a body already archived elsewhere. Resume must unpack to plain,
  or fail with a message that says which tool is missing. Check whether the
  newest sessions are the ones kept and whether equal timestamps are ordered
  deterministically.
- Search and listing: `/sessions` reads sidecars alone; search reads what it
  needs, including archived bodies, and reports hits with context. Check case,
  unicode, a term in a title versus a body, a query with regex/punctuation,
  many sessions, and a body that is still being appended.
- Compaction on disk: the loader starts from the last `axe_compact` marker,
  returns the messages after it, strips the marker key from the request, and
  leaves the pre-marker bytes searchable. Check two markers, a marker with a
  zero count, and a marker whose summary is empty.
- Per-directory state: `state.json` fields (model, thinking, thoughts, explore
  if stored), a missing or partial file, an unknown key, and an atomic save.
  The model selection order must be directory state, then `default.json`, then
  config; `/model -g` must write the default and drop the directory's pin.
  Check a stale model id, a provider that disappeared, and a relative
  `state.json` path.
- Prompt history: newest 500, 128 KB rewrite, `0600`, the append fast path, the
  tail read, a blank line, a line starting with a space, and an immediate
  repeat are not steps. Check unicode, a very long line, two axe instances
  appending at once, and a corrupt line in the middle.
- Naming and `/new`: naming runs once after the first answer, in the
  background; a switch or quit while it is in flight must not name the wrong
  session; `/new` names the old one, clears the screen and its scrollback, and
  starts read-only. Check the startup chooser's recent list, resume failure,
  and a piped run that has no chooser.

For each finding give `file:line`, the invariant that breaks, and the failure
scenario in bytes on disk: which file is left half-written or unreadable, what
the list shows that the body does not, or what a resume sends that the live
session did not.

Out of scope: message ordering inside a turn (see `turn-and-history.md`), the
compaction prompt itself (same), file modes as a sandbox boundary (see
`sandbox.md`), and startup latency (see `performance-and-snappiness.md`).
