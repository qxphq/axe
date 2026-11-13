# System prompt and skills

Axe review prompt — one concern: **the prefix is byte-stable for a session, its
instructions say exactly what axe does, in the documented order, and the skills
catalogue gives the model the smallest true picture of what is installed.**

Read first: `AGENTS.md` invariants 5–6 and 10; README "Instructions", "Skills",
"Modes"; `src/prompt.rs` in full, `src/tools.rs` (`BEHAVIOR`,
`system_prompt`, `Mode::note`), `src/config.rs` (`read_agents`, `find_agents`,
`parse_skills`), `src/skills.rs` in full, `src/startup.rs` (prompt assembly),
and `src/main.rs` (`base_messages`, `skill_listing`, `set_mode`, `new_session`).

Verify with depth — compare the rendered prompt and the code it describes:

- The built-in rules: one line each, policy not workflow, no project- or
  provider-specific content, numbered so a later rule can refer back. Check the
  cwd line and the mode paragraph for claims that are false in one mode (for
  example "read the web" when `fetch` is refused). Check that tool names,
  descriptions and schemas are absent from the prompt and present only in the
  `tools` array.
- Append order: `agents` (config), then the nearest `AGENTS.md` at or above
  cwd, then `system`, then the skills catalogue. Check the wrapper
  `<project_instructions path="...">`, path escaping (`"` and control bytes),
  a file containing the closing tag, the 48 KB cap (bytes versus chars,
  truncation marker or silent cut), and a missing/unreadable file. Check
  `find_agents` stops where the README says (repository root, `.git`, home,
  root, symlinks) and finds the nearest, not the highest.
- Stability: the prefix is built once at startup and never rewritten; mode
  travels as a `user` note, not in the prefix; skills and MCP catalogues sit
  last; a mid-session `/explore`, `/model`, `/sessions` or resize changes
  nothing above. A resume rebuilds the same prefix from current instructions;
  check a changed `AGENTS.md` or a new skill on resume (new session boundary)
  and whether the model ever sees two different prefixes in one session.
- Tool array: `definitions()` is a `OnceLock`; MCP tools are appended once;
  a mode flip leaves the array unchanged (and the note explains the tool set);
  an MCP restart does not rebuild it. Check the order (built-ins first), the
  descriptions, and whether the README's seven-tool table matches the array.
- Skills discovery: locations and precedence (project at or above cwd up to
  the repository root, global config dir, `~/.agents/skills`, config paths);
  recursive scan, stop at a directory holding `SKILL.md`, skip `.git`,
  `node_modules`, `target` and dot directories; first name wins; 200 skills and
  16 KB catalogue caps. Check a symlinked directory, an unreadable file, an
  invalid name, a missing description, a body with frontmatter, and a skill
  path that points at a `SKILL.md`.
- Frontmatter: the documented subset (`name`, `description`,
  `disable-model-invocation`, `allowed-tools`); plain, quoted and block
  scalars; a field axe does not know; a duplicate name; a name that violates
  the format. Check what `/skills` shows, what `/skill:name` loads, and that
  `disable-model-invocation` removes it from the catalogue while keeping the
  command.
- Activation: `/skill:name` reads the body at use time, wraps it, runs a turn,
  and handles an empty body or a missing file. Check the catalogue's absolute
  path is the one the model can actually `read`, and that a skill is not
  silently re-read at startup.
- The mode note: its words live in `Mode::note`, not in the prompt; a flip
  appends one note; `/new` starts with explore on; a resume replays the last
  note from the history rather than inventing one. Check the note's claim that
  the plan directory exists (it is named only in a denial, by design) and that
  a model in explore mode is told to say `/explore off` rather than attempt a
  write.
- Size: measure the prefix for a fresh directory, this repo, a directory with a
  48 KB `AGENTS.md`, 200 skills, and one MCP server. Identify sections that are
  paid every round without per-turn value and any duplication with the README
  or the tool descriptions; hand the measurement to `token-efficiency.md`
  rather than fixing it here.

For each finding give the section and its position in the prompt, `file:line`,
the invariant or README sentence it violates, and the concrete outcome: a model
told it can do something it cannot, instructions from the project treated as
authority, a prefix that differs between two requests in one session, or a
skill that is listed but unreadable.

Out of scope: token cost and cacheability as numbers (see
`token-efficiency.md`), MCP catalogue and trust (see `config-and-mcp.md`), and
tool behavior (see `tool-contract.md`).
