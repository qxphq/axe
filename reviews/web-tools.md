# Web access: fetch and search

Axe review prompt — one concern: **`fetch` and `web_search` reach only the
hosts they intend, return the page as safe labelled text within the caps, and
never make a failure look like content.**

Read first: `AGENTS.md` invariant 7, README "Tools" and "Trust and security";
then `src/web.rs` in full, `src/http.rs` (URL parse, redirects, body caps),
`src/text.rs` (control stripping and truncation), and the `fetch:` fixtures in
`mock.py`/`harness.py` (`plain`, `inject`, `big`, `image`, `redirect`,
`latin1`, `slow`, paging, `--no-fetch-local`).

Verify with depth — follow one URL from string to model result:

- The host guard: scheme `http`/`https` only, the local/private/link-local
  refusal, DNS resolution and whether the checked address is the one connected
  to (rebinding), an IP literal in decimal/octal/hex/`[::]` form, a host with a
  trailing dot, uppercase, userinfo (`user:pass@host`), a non-default port,
  and an IPv6 zone. Check the debug/test knob `AXE_FETCH_ALLOW_LOCAL` is
  compiled out of a release and that the harness path cannot be reached
  otherwise.
- Redirects: whether they are followed at all, cross-host, from http to https
  and back, a loop, a relative `Location`, a `Location` with a fragment, and a
  redirect to a private address. The host guard must apply to the final target,
  not just the first.
- Caps and paging: 1 MiB off the wire, 50 000 chars to the model, `start`
  1-based, `chars` at most 50 000. Check `start` beyond the end, zero, negative,
  non-numeric, a `chars` larger than the rest, and two pages that overlap or
  leave a gap. Check a server that lies about `Content-Length`, streams
  forever, sends a huge single line, or closes early. A page that grows between
  pages must not corrupt the offsets.
- HTML to text: comments, `script`/`style`/`noscript`, `pre`, `br`, block
  elements, `alt` text, tables, a `<title>`, an unclosed tag, a malformed
  attribute, a `<` in text, a character reference, and a page in Latin-1
  (charset from the header or a meta tag). Check that the text keeps paragraph
  breaks and does not glue words.
- Sanitizer: terminal controls, C1, bidi overrides, zero-width characters,
  chat-template tokens (`<|im_start|>`, `[INST]`, role prefixes), and a line
  that begins like axe's own `source:` label. Check that persuasion is not
  claimed to be removed, only controls and formatting.
- `web_search`: Brave when a key resolves, DuckDuckGo otherwise, and the
  fallback when Brave errors or returns nothing. Check the query encoding,
  `count` default 10 and clamp 1–20, the result shape (title, URL, snippet),
  de-duplication, a redirect wrapper (`uddg=`), an HTML entity in a title, a
  result with no URL, a no-results page, a captcha/blocked page, and the
  engine line at the top. Check that a search result is labelled untrusted as
  a whole, not per field.
- The source label: `source: URL (untrusted)` must appear once at the top,
  carry the final URL, and be impossible for page content to close or forge.
  Check a URL with a newline or control byte in it (it must be stripped before
  the label is built) and a page whose first line is also a source label.
- Failures: a refused host, a DNS failure, a TLS certificate error, a 404, a
  500 with a body, a timeout, a cancelled call, and a page that decodes to
  nothing. Each must be a result the model can act on, with a distinguishable
  reason, and must not be truncated into a misleading success.
- The fixtures: for each page, name the behavior it pins; for the ones that
  are missing (redirect chains, DNS rebinding, charset, gzip, an oversized
  stream), say so. Check `--no-fetch-local` actually refutes the local page
  and that the harness's fixture server is not accidentally reachable in a
  release build.

For each finding give the URL or page bytes, `file:line`, and the concrete
outcome: a request to a host the tool promised not to reach, text that loses
its content or carries an escape sequence, a paging window that duplicates or
drops text, or an error the model reads as a successful fetch.

Out of scope: general HTTP framing and SSE (see `http-and-sse.md`), terminal
rendering of the result (see `rendering.md`), and the trust model's broader
claims (see `trust-and-secrets.md`).
