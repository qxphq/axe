# HTTP client, SSE, and the provider contract

Axe review prompt — one concern: **the client speaks exactly enough HTTP/1.1
and SSE for any OpenAI-compatible provider, never hard-codes a provider, and
every refusal or truncation is reported instead of being mistaken for a reply.**

Read first: `AGENTS.md` invariants 4 and 11, README "Config" (both tables),
"Run" (piped), and the provider notes at the end of `AGENTS.md`. Then
`src/http.rs` in full, `src/api.rs` in full, `src/config.rs` (`ProviderConfig`,
`parse_provider`), and `mock.py` (which behaviors the fixtures do and do not
cover).

Verify with depth — walk the bytes, do not skim:

- URL parsing: scheme, host, port, path, query, fragment, userinfo, bracketed
  IPv6, uppercase host, trailing dot, IDN, a base URL with or without a
  trailing slash. `chat/completions` and `/models` must hang under the base
  exactly once. Check the error text and which fields are validated.
- The request: request line and target, `Host`, `Authorization` (only when a
  key resolves), `Content-Type`, `Accept`, `Content-Length`, the per-provider
  `session_header` and its stability across rounds and resume, and any header
  whose value could contain a newline. Check an empty body, a very large body,
  and a body with non-ASCII.
- TLS: rustls configuration, webpki roots, SNI, a certificate failure, an
  expired certificate, a hostname mismatch, and an `http://` base URL
  (plaintext is allowed for a local provider). Check that no provider name
  decides a code path.
- The response: status line, header size/line cap, folded or duplicate
  headers, `1xx`, `204`, `304`, `HEAD`-like no-body statuses, `Content-Length`
  that lies, chunked with extensions and trailers, close-delimited bodies, and
  a body over the cap. A non-200 must return the provider's body as an error,
  not an empty success.
- Line reading: `read_line` and `read_line_raw` must handle LF, CRLF, a line
  split across reads, a line over the cap, a final line without a newline, and
  invalid UTF-8. Check the failure mode when the peer closes mid-line.
- SSE: `data:` with one space, no space, or none; several `data:` lines joined
  by `\n`; `event:`, `id:`, `retry:` and comment keep-alives; a blank line as
  the dispatch boundary; `[DONE]`; a JSON object split across chunks; an
  unknown or malformed JSON line; a stream that ends without `[DONE]`. Check
  the API's own loop: it currently trims and requires `data:` with no
  multi-line joining — say whether that is a real provider gap.
- Tool calls: `index`, `id`, `type`, `function.name`, `function.arguments`
  split in pieces, several parallel calls, a call whose name arrives after the
  arguments, a duplicate index, a call with no `id`, an argument string that is
  not valid JSON, and an empty argument object. The assembler must produce one
  call per index with the arguments exactly concatenated.
- Chat body: `model`, `messages`, `stream`, `tools`, `stream_options`, the
  thinking level merge, `temperature`, and the absence or presence of
  `max_tokens`. Check that `extra` is merged last and cannot overwrite
  `messages` or `tools` accidentally, that `{}` leaves the provider default,
  and that an unknown level falls back rather than sending nonsense.
- Models: `models_url`, auth header, the `data[].id` shape, a provider that
  reports `context_window` in any of the documented shapes, a malformed list,
  a 401, and one provider failing while others start. The splash must not wait
  forever or abort the session.
- Usage and reasoning: the final usage chunk, a usage chunk with no choices, a
  missing `usage` field, `prompt_tokens_details`/`cached_tokens` (if parsed),
  `completion_tokens`, and `reasoning_content` deltas. Check whether the
  request sends `include_usage` and whether the reply keeps what later rounds
  need.
- Cancellation: `Cancel` closing the socket mid-body, during TLS, during a
  line read, and before connect; the thread must finish and the caller must
  see a cancellation, not a truncated success. Check the timeout paths
  (`request_with_timeout`) and whether a timeout is distinguishable from a
  reset.
- Fixture honesty: for each behavior `mock.py` covers, name the test; for each
  behavior above it does not cover (TLS, gzip, chunk extensions, multi-line
  data, malformed JSON, huge headers), say so. A passing suite that cannot
  express the failure is a gap.

For each finding give the request or response bytes at `file:line`, the
provider behavior it breaks, and the user-visible symptom: a turn that fails
with a misleading error, a silently dropped tool call, a truncated answer
treated as complete, or a provider that cannot be configured without code.

Out of scope: what the messages mean and how they are stored (see
`turn-and-history.md`), MCP's use of HTTP (see `config-and-mcp.md`), web
fetching's host guard and HTML reduction (see `web-tools.md`), and prompt
cacheability (see `token-efficiency.md`).
