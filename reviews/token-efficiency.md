# Token efficiency and context economics

Axe review prompt — one concern: **axe spends as few input and output tokens as
the session needs, keeps the prefix cacheable, and knows what its context holds
instead of estimating from bytes.**

Read first: `AGENTS.md` invariants 4–7 (provider-agnostic, stable prefix, tools
are dumb), README "Config", "Instructions", "State and sessions", "Tools", "MCP
servers", "REPL"; then `src/prompt.rs`, `src/tools.rs` (`BEHAVIOR`, `SPECS`,
`definitions`, the truncation constants), `src/skills.rs` (catalogue and caps),
`src/api.rs` (`chat`, `stream_chat`, `Usage`), `src/turn.rs`
(`assistant_message`, usage recording), `src/main.rs` (`compact`,
`compaction_prompt`, `naming_excerpt`, `history_bytes`, `fill`), `src/store.rs`,
and `src/mcp.rs` (`definitions`, schema caps).

Then mine the two references for behavior, not code:

- `references/pi/packages/ai/src/api/openai-completions.ts` and
  `anthropic-messages.ts`: `prompt_cache_key`, `cache_control`, retention,
  usage fields (`prompt_tokens_details`, `prompt_cache_hit_tokens`).
- `references/pi/packages/coding-agent/src/core/compaction/`: trigger, keep and
  reserve token settings, file-operation tracking, summarization prompt.
- `references/opencode/packages/core/src/session/compaction.ts`,
  `model-request.ts`, `runner/llm.ts`, and the tool plugins under
  `references/opencode/packages/core/src/tool/`: measured-vs-estimated context,
  precompute, overflow shrink, output limits, truncation with a continuation
  hint.

Verify with depth — measure the bytes and tokens, do not only read:

- Request shape and cacheability: system prompt and tools first, volatile
  content last; anything inside a session that changes the prefix (mode, cwd,
  time, model switch, resumed instructions, MCP restart); `session_header`
  stability; the skills catalogue appended last. Name which provider families
  cache a stable prefix for free and which need request metadata, and whether
  the "differences are config, never per-provider code paths" rule admits
  `prompt_cache_key`, `cache_control`, `prompt_cache_retention`, or `store`.
- Usage accounting: parse cached-read and cache-write tokens as well as prompt,
  completion and reasoning tokens. Check what the info row's fill means, whether
  compaction decides on measured usage or the byte/4 fallback, and whether
  `used` is reset when compaction or `/new` shrinks the history. Compare
  `history_bytes` with the actual serialized request: JSON overhead, tool
  results, code, and non-Latin text.
- Compaction: `/compact` is manual; there is no trigger before the provider
  rejects an oversized request. The request is half the window in characters,
  capped at 200 000, drops the oldest lines, and cuts each message head/tail.
  Check the summary request's own cost and output ceiling, whether a second
  compaction summarizes a summary, whether file paths and decisions survive,
  and whether `/compact` can itself overflow. Weigh opencode's due/precompute/
  shrink and pi's keep-recent/reserve/file-list behavior against axe's
  append-only marker (invariant 6); name the smallest borrow that fits.
- Tool results, the largest input line item: `bash` 30 000 chars, `read` and
  `fetch` 50 000, MCP 30 000 with the whole text in a file. At four characters
  per token that is 7 500–12 500 tokens per call. Check whether the model is
  told how to continue (`offset`, `start`, the file path), whether truncation
  is head-and-tail with a marker, whether the marker says how much was cut, and
  whether one read-heavy turn can be quantified. Say whether a smaller default
  window is justified and which check would regress.
- Output tokens: no `max_tokens` on regular requests, no round limit, no
  recovery for tool-call arguments cut by a length stop. Check `finish_reason`
  reporting and what a runaway answer costs. Decide whether an output ceiling
  or a step limit is a token fix or a product decision.
- History growth: every tool result stays in the body and every request; stale
  output is never pruned outside compaction. `assistant_message` re-sends
  `reasoning_content`: find which providers require it and what it costs each
  round. Mode notes and shell output join as user messages. Compare opencode's
  projection and pi's measure-plus-estimate with what axe keeps.
- Always-paid sections: the built-in rules and mode paragraph, config
  instructions, the 48 KB `AGENTS.md` cap, the 16 KB skills catalogue (200
  skills), the seven tool schemas plus MCP's 128 tools / 256 KiB schema per
  server, and the full README returned by `about`. Measure a fresh session in
  this repo and one with a configured MCP server. Identify duplication (the
  README and AGENTS.md restating each other, descriptions repeated in the
  prompt) and any always-paid section that earns no per-turn value.
- Idiomatic request fields: `stream_options.include_usage`, the `tools` shape,
  tool-call ids, reasoning fields, `max_completion_tokens` versus `max_tokens`,
  parallel tool calls, and `store` where a provider defaults to retaining the
  request. Each is a config value or a one-line decision, never a new provider
  branch. Name missing fields axe is charged for and fields it sends
  unconditionally that it should not.
- Reference mining: list each borrowable trick with a verdict — **adopt**
  (fits axe, small, objectively determined), **consider** (needs a user or
  policy decision), **reject** (contradicts an invariant, adds a dependency, or
  is provider-specific). At minimum cover cache metadata, cached-token
  accounting, measured-usage auto-compaction, keep-recent-token windows,
  background precompute, overflow shrink, continuation hints, output ceilings,
  and preservation of file operations. Check a reference's license before
  proposing copied code; describe behavior, not a patch.
- Session lifecycle: title, summary and naming requests, the naming excerpt, and
  mode notes — count their tokens. Check that naming runs once, that a failed
  naming does not retry every round, and that resume rebuilds the same prefix
  rather than a longer or differently ordered one.

For each finding, give the bytes or tokens on the wire (measured, or a labelled
estimate), the request or round it lands in, `file:line`, and the user-visible
cost: a more expensive turn, an earlier compaction, or a context that could
have held another file. Close with a ranked table of saving, effort, risk, and
whether the fix is objective or needs a prompt-policy decision. Prefer one
measured example per claim to a catalogue of micro-savings.

Out of scope: latency, CPU and repaint cost (see
`performance-and-snappiness.md`), HTTP/SSE and compaction-marker correctness
(see `http-and-sse.md`, `turn-and-history.md`), prompt wording and instruction
order as such (see `prompt-prefix.md`; here only their token cost and
cacheability), and provider protocol bugs (see `http-and-sse.md`).
