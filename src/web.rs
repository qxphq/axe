//! Web access: `fetch` and `web_search`.
//!
//! One module so the HTTP target guards, HTML-to-text tokenizer, sanitizer and
//! Brave/DuckDuckGo parsers stay together. Everything here is read-only; the
//! result is untrusted text for the model.

use std::net::{IpAddr, ToSocketAddrs};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use serde_json::Value;

use crate::http::{self, Cancel};
use crate::markdown;

/// Caps on a fetched page: bytes off the wire, characters the model sees, and
/// wall time spent reading the body.
const FETCH_BYTES: u64 = 1 << 20;
const FETCH_CHARS: usize = 50_000;
const FETCH_SECONDS: u64 = 60;

/// Caps on a search: bytes off the wire, results asked for, and wall time
/// spent reading the body.
const SEARCH_BYTES: u64 = 1 << 20;
const SEARCH_SECONDS: u64 = 30;
const SEARCH_RESULTS: u64 = 10;
const SEARCH_LIMIT: u64 = 20;

static BRAVE_KEY: OnceLock<Option<String>> = OnceLock::new();

/// The Brave token loaded from the config. Set once: the config is read once
/// per process, and a missing or empty token means DuckDuckGo.
pub fn set_brave_key(key: Option<String>) {
    let _ = BRAVE_KEY.set(key);
}

pub fn fetch(args: &Value, cancel: &Cancel) -> String {
    let Some(url) = args["url"].as_str() else {
        return "error: missing \"url\"".into();
    };
    let url = url.trim();
    if !(url.starts_with("http://") || url.starts_with("https://")) {
        return format!("error: fetch: url must start with http:// or https://: {url}");
    }
    // `split_url` reads the authority up to the last ':', which cannot express a
    // bracketed literal, and the v6 ranges this tool would not cover are worse
    // than an honest refusal.
    if url
        .split_once("://")
        .is_some_and(|(_, rest)| rest.starts_with('['))
    {
        return "error: fetch: ipv6 literals are not supported".into();
    }
    let (mut ep, target) = match http::split_url(url) {
        Ok(pair) => pair,
        Err(e) => return format!("error: fetch: {e}"),
    };
    // `request` puts `prefix` in front of the path it is given; the target is
    // already whole, so the endpoint contributes nothing to it.
    ep.prefix.clear();
    // The harness needs loopback; a release build must not let an environment
    // variable turn the SSRF guard off, so the knob exists only in debug.
    let local =
        cfg!(debug_assertions) && std::env::var("NANO_FETCH_ALLOW_LOCAL").is_ok_and(|v| v == "1");
    if !local {
        if let Err(e) = guard_host(&ep.host) {
            return format!("error: fetch: {e}");
        }
    }

    let mut response = match http::request_with_timeout(
        &ep,
        "GET",
        &target,
        None,
        &[],
        None,
        Some(cancel),
        Duration::from_secs(FETCH_SECONDS),
    ) {
        Ok(response) => response,
        Err(e) => return format!("error: fetch: {e}"),
    };
    let content_type = response
        .content_type
        .as_deref()
        .unwrap_or("")
        .to_ascii_lowercase();
    // A redirect is reported, never followed: one call, one host.
    if (300..400).contains(&response.status) {
        let location = response.location.as_deref().unwrap_or("(none)");
        return format!(
            "error: fetch: http {} -> location: {location}",
            response.status
        );
    }
    if !(200..300).contains(&response.status) {
        return format!("error: fetch: http {} from {url}", response.status);
    }
    if !["text/", "json", "xml", "markdown"]
        .iter()
        .any(|kind| content_type.contains(kind))
    {
        return if content_type.is_empty() {
            "error: fetch: no content-type".into()
        } else {
            format!("error: fetch: not text: {content_type}")
        };
    }

    let deadline = Instant::now() + Duration::from_secs(FETCH_SECONDS);
    let body = match response.read_capped(FETCH_BYTES, deadline) {
        Ok(body) => body,
        Err(e) => return format!("error: fetch: read: {e}"),
    };
    let text = sanitize(&extract_text(&body));
    page_text(&text, args)
}

/// Slice, then cut: `start` and `chars` address the extracted text, so a page
/// over the cap can still be read past it.
fn page_text(text: &str, args: &Value) -> String {
    let total = text.chars().count();
    if total == 0 {
        return "error: fetch: page has no text".into();
    }
    let start = args["start"].as_u64().unwrap_or(1).max(1) as usize;
    if start > total {
        return format!("error: fetch: start {start} is past the end ({total} characters)");
    }
    let chars = args["chars"].as_u64().unwrap_or(FETCH_CHARS as u64) as usize;
    let chars = chars.clamp(1, FETCH_CHARS);
    let head: String = text.chars().skip(start - 1).take(chars).collect();
    let shown = head.chars().count();
    let body = if start - 1 + shown < total {
        let next = start + shown;
        format!(
            "{head}\n...[truncated after {shown} of {total} characters; use start {next} to continue]..."
        )
    } else {
        head
    };
    // The model is told where this text came from; the header is a label, not
    // a guarantee, and the page stays byte-for-byte as extracted.
    format!("source: web (untrusted)\n{body}")
}

/// One search hit, already plain text: `sanitize` has run on every field.
struct Hit {
    title: String,
    url: String,
    snippet: String,
}

pub fn web_search(args: &Value, cancel: &Cancel) -> String {
    let Some(query) = args["query"]
        .as_str()
        .map(str::trim)
        .filter(|q| !q.is_empty())
    else {
        return "error: missing \"query\"".into();
    };
    let count = args["count"]
        .as_u64()
        .unwrap_or(SEARCH_RESULTS)
        .clamp(1, SEARCH_LIMIT) as usize;

    // Brave is the default when it has a token. Any reason it cannot answer —
    // no results, an HTTP error, a page that is not the JSON we expect — is a
    // reason to ask DuckDuckGo the same question.
    let mut notes = Vec::new();
    if let Some(key) = BRAVE_KEY
        .get()
        .and_then(|key| key.as_deref())
        .filter(|key| !key.is_empty())
    {
        match brave(query, count, key, cancel) {
            Ok(hits) if !hits.is_empty() => return results("brave", hits, count),
            Ok(_) => notes.push("brave: no results".to_string()),
            Err(e) => notes.push(format!("brave: {e}")),
        }
    }
    match ddg(query, count, cancel) {
        Ok(hits) if !hits.is_empty() => return results("duckduckgo", hits, count),
        Ok(_) => notes.push("duckduckgo: no results".to_string()),
        Err(e) => notes.push(format!("duckduckgo: {e}")),
    }
    format!("error: web_search: {}", notes.join("; "))
}

fn results(source: &str, hits: Vec<Hit>, count: usize) -> String {
    let mut out = format!("source: {source} (untrusted)\n");
    for (i, hit) in hits.into_iter().take(count).enumerate() {
        out.push_str(&format!("{}. {}\n   {}\n", i + 1, hit.title, hit.url));
        if !hit.snippet.is_empty() {
            out.push_str(&format!("   {}\n", hit.snippet));
        }
    }
    crate::tools::truncate(out, 30_000)
}

const BRAVE_SEARCH: &str = "https://api.search.brave.com/res/v1/web/search";

fn brave(query: &str, count: usize, key: &str, cancel: &Cancel) -> Result<Vec<Hit>, String> {
    let url = format!("{BRAVE_SEARCH}?q={}&count={count}", escape(query));
    let headers = [("X-Subscription-Token".to_string(), key.to_string())];
    let body = get(&url, &headers, cancel)?;
    let json: Value = serde_json::from_str(&body).map_err(|e| format!("bad JSON: {e}"))?;
    Ok(parse_brave(&json, count))
}

fn parse_brave(json: &Value, count: usize) -> Vec<Hit> {
    let mut hits = Vec::new();
    if let Some(results) = json["web"]["results"].as_array() {
        for item in results {
            let Some(url) = item["url"].as_str().and_then(http_url) else {
                continue;
            };
            let title = fragment(item["title"].as_str().unwrap_or(""));
            let snippet = fragment(item["description"].as_str().unwrap_or(""));
            hits.push(Hit {
                title: if title.is_empty() { url.clone() } else { title },
                url,
                snippet,
            });
            if hits.len() == count {
                break;
            }
        }
    }
    hits
}

const DDG_SEARCH: &str = "https://html.duckduckgo.com/html/";

fn ddg(query: &str, count: usize, cancel: &Cancel) -> Result<Vec<Hit>, String> {
    let url = format!("{DDG_SEARCH}?q={}", escape(query));
    let html = get(&url, &[], cancel)?;
    Ok(parse_ddg(&html, count))
}

fn parse_ddg(html: &str, count: usize) -> Vec<Hit> {
    let lower = html.to_ascii_lowercase();
    let mut hits: Vec<Hit> = Vec::new();
    // The hit a following snippet belongs to. An ad link has no result URL,
    // so it clears this instead of leaving the previous hit open.
    let mut pending: Option<usize> = None;
    let mut i = 0;
    while let Some(open) = lower[i..].find('<') {
        i += open;
        if lower[i..].starts_with("<!--") {
            i = match lower[i + 4..].find("-->") {
                Some(end) => i + 4 + end + 3,
                None => break,
            };
            continue;
        }
        let closing = lower[i + 1..].trim_start().starts_with('/');
        let Some(end) = tag_end(&lower, i) else {
            break;
        };
        let name = tag_name(&lower, i, end);
        let tag = &html[i..end];
        i = end + 1;
        if closing {
            continue;
        }
        if name == "script" || name == "style" {
            i = match region_end(&lower, i, &format!("</{name}")) {
                Some(next) => next,
                None => break,
            };
            continue;
        }
        let class = attr(tag, "class").unwrap_or_default();
        let result = has_class(&class, "result__a");
        let snippet = has_class(&class, "result__snippet");
        if !result && !snippet {
            continue;
        }
        // Skip the element's own body: tags in it are part of the snippet or
        // the title, not another result.
        let Some((close, next)) = element_close(&lower, i, &name) else {
            break;
        };
        let text = fragment(&html[i..close]);
        i = next;
        if result {
            match attr(tag, "href").as_deref().and_then(url_of) {
                Some(url) => {
                    hits.push(Hit {
                        title: if text.is_empty() { url.clone() } else { text },
                        url,
                        snippet: String::new(),
                    });
                    pending = Some(hits.len() - 1);
                }
                None => pending = None,
            }
        } else if let Some(index) = pending.take() {
            hits[index].snippet = text;
        }
    }
    hits.truncate(count);
    hits
}

/// One fixed host, the same guard and cap `fetch` uses. The caller supplies
/// the full URL; the endpoint's prefix is cleared so its path is not doubled.
fn get(url: &str, headers: &[(String, String)], cancel: &Cancel) -> Result<String, String> {
    let (mut ep, target) = http::split_url(url)?;
    ep.prefix.clear();
    guard_host(&ep.host)?;
    let mut response = http::request_with_timeout(
        &ep,
        "GET",
        &target,
        None,
        headers,
        None,
        Some(cancel),
        Duration::from_secs(SEARCH_SECONDS),
    )?;
    // One call, one host: a redirect is reported, never followed.
    if (300..400).contains(&response.status) {
        let location = response.location.as_deref().unwrap_or("(none)");
        return Err(format!("http {} -> location: {location}", response.status));
    }
    if !(200..300).contains(&response.status) {
        return Err(format!("http {} from {}", response.status, ep.host));
    }
    let deadline = Instant::now() + Duration::from_secs(SEARCH_SECONDS);
    response
        .read_capped(SEARCH_BYTES, deadline)
        .map_err(|e| format!("read: {e}"))
}

/// Percent-encode a query: UTF-8 bytes, unreserved characters pass through.
fn escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for b in text.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// Percent-decode an `uddg` value. `+` is left alone: it is legal in a URL.
fn unescape(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let (Some(hi), Some(lo)) = (hex(bytes[i + 1]), hex(bytes[i + 2])) {
                out.push((hi << 4) | lo);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn hex(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

/// The href of a DuckDuckGo result, with its `//duckduckgo.com/l/?uddg=...`
/// redirect unwrapped. Ads and other DuckDuckGo-hosted links are not results.
fn url_of(href: &str) -> Option<String> {
    let href = markdown::decode_entities(href);
    let href = href.trim();
    let full = match href.strip_prefix("//") {
        Some(rest) => format!("https://{rest}"),
        None => href.to_string(),
    };
    let url = if is_ddg(&full) {
        uddg(&full).unwrap_or(full)
    } else {
        full
    };
    if is_ddg(&url) {
        return None;
    }
    http_url(&url)
}

fn is_ddg(url: &str) -> bool {
    host_of(url).is_some_and(|host| host == "duckduckgo.com" || host.ends_with(".duckduckgo.com"))
}

fn host_of(url: &str) -> Option<&str> {
    let rest = url.split_once("://")?.1;
    rest.split(['/', '?', '#']).next()
}

fn uddg(url: &str) -> Option<String> {
    let query = url.split_once('?')?.1;
    query
        .split('&')
        .find_map(|pair| pair.strip_prefix("uddg=").map(unescape))
}

fn http_url(raw: &str) -> Option<String> {
    let url = sanitize(raw).trim().to_string();
    if !(url.starts_with("https://") || url.starts_with("http://")) {
        return None;
    }
    if url.chars().any(char::is_whitespace) {
        return None;
    }
    Some(url)
}

/// JSON text as one sanitized line.
fn flat(text: &str) -> String {
    sanitize(text)
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

/// HTML fragment as one sanitized line.
fn fragment(html: &str) -> String {
    flat(&extract_text(html))
}

fn has_class(class: &str, name: &str) -> bool {
    class
        .split_whitespace()
        .any(|c| c.eq_ignore_ascii_case(name))
}

/// Value of `name` in a tag body (the text between `<` and `>`). Attribute
/// names are ASCII and case-insensitive; values keep their case.
fn attr(tag: &str, name: &str) -> Option<String> {
    let mut i = tag.find(char::is_whitespace).unwrap_or(tag.len());
    while i < tag.len() {
        while i < tag.len() && tag.as_bytes()[i].is_ascii_whitespace() {
            i += 1;
        }
        let start = i;
        while i < tag.len() {
            let b = tag.as_bytes()[i];
            if b.is_ascii_whitespace() || b == b'=' || b == b'/' {
                break;
            }
            i += 1;
        }
        if i == start {
            i += 1;
            continue;
        }
        let key = &tag[start..i];
        while i < tag.len() && tag.as_bytes()[i].is_ascii_whitespace() {
            i += 1;
        }
        if i >= tag.len() || tag.as_bytes()[i] != b'=' {
            continue;
        }
        i += 1;
        while i < tag.len() && tag.as_bytes()[i].is_ascii_whitespace() {
            i += 1;
        }
        if i >= tag.len() {
            break;
        }
        let quote = tag.as_bytes()[i];
        let value;
        if quote == b'"' || quote == b'\'' {
            i += 1;
            let from = i;
            while i < tag.len() && tag.as_bytes()[i] != quote {
                i += 1;
            }
            value = &tag[from..i];
            if i < tag.len() {
                i += 1;
            }
        } else {
            let from = i;
            while i < tag.len()
                && !tag.as_bytes()[i].is_ascii_whitespace()
                && tag.as_bytes()[i] != b'>'
            {
                i += 1;
            }
            value = &tag[from..i];
        }
        if key.eq_ignore_ascii_case(name) {
            return Some(value.to_string());
        }
    }
    None
}

/// Start of `</name` at or after `from`, and the index just past that closing
/// tag. `None` when the element never closes.
fn element_close(lower: &str, from: usize, name: &str) -> Option<(usize, usize)> {
    let marker = format!("</{name}");
    let at = from + lower[from..].find(&marker)?;
    let end = at + lower[at..].find('>')?;
    Some((at, end + 1))
}

/// Refuse the hosts a fetched URL must never reach: this machine, the local
/// network, and the metadata endpoints. A resolve-and-check, not a sandbox:
/// `connect` resolves the name again, so a rebinding DNS can walk past it.
fn guard_host(host: &str) -> Result<(), String> {
    let name = host.trim_end_matches('.').to_ascii_lowercase();
    if name.is_empty() {
        return Err("empty host".into());
    }
    for suffix in ["localhost", "local", "internal"] {
        if name == suffix || name.ends_with(&format!(".{suffix}")) {
            return Err(format!("refused host: {host}"));
        }
    }
    let addrs = (name.as_str(), 80u16)
        .to_socket_addrs()
        .map_err(|e| format!("cannot resolve {host}: {e}"))?;
    let mut seen = false;
    for addr in addrs {
        seen = true;
        if local_address(addr.ip()) {
            return Err(format!("refused host: {host} resolves to {}", addr.ip()));
        }
    }
    if !seen {
        return Err(format!("cannot resolve {host}"));
    }
    Ok(())
}

fn local_address(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            let [a, b, ..] = v4.octets();
            v4.is_loopback()
                || v4.is_private()
                || v4.is_link_local()
                || v4.is_multicast()
                || v4.is_broadcast()
                || v4.is_unspecified()
                || a == 0
                // 100.64.0.0/10, carrier-grade NAT.
                || (a == 100 && (64..128).contains(&b))
        }
        IpAddr::V6(v6) => {
            let head = v6.segments()[0];
            v6.is_loopback()
                || v6.is_unspecified()
                || v6.is_multicast()
                || head & 0xfe00 == 0xfc00 // fc00::/7, unique local
                || head & 0xffc0 == 0xfe80 // fe80::/10, link local
                || v6.to_ipv4_mapped().map(IpAddr::V4).is_some_and(local_address)
        }
    }
}

/// Strip what a terminal or another tokenizer can act on: escapes, invisible
/// characters, chat-template tokens and role headers. A channel remover, not a
/// boundary.
fn sanitize(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\u{1b}' {
            match chars.next() {
                Some('[') => {
                    // CSI: parameters, then a final byte in 0x40..=0x7e.
                    for c in chars.by_ref() {
                        if ('\u{40}'..='\u{7e}').contains(&c) {
                            break;
                        }
                    }
                }
                Some(']') => {
                    // OSC: ends at BEL or ST.
                    while let Some(c) = chars.next() {
                        if c == '\u{7}' {
                            break;
                        }
                        if c == '\u{1b}' && chars.peek() == Some(&'\\') {
                            chars.next();
                            break;
                        }
                    }
                }
                _ => {}
            }
            continue;
        }
        if c == '\u{2028}' || c == '\u{2029}' {
            out.push('\n');
        } else if is_invisible(c) || (c.is_control() && c != '\n' && c != '\t') {
            continue;
        } else {
            out.push(c);
        }
    }
    neutralize(&out)
}

/// Zero-width and format characters, the tag block, and bidi controls.
fn is_invisible(c: char) -> bool {
    matches!(
        c as u32,
        0x00AD
            | 0x180E
            | 0x200B..=0x200F
            | 0x202A..=0x202E
            | 0x2060..=0x2064
            | 0x2066..=0x2069
            | 0xFEFF
            | 0xE0000..=0xE007F
    )
}

fn neutralize(text: &str) -> String {
    let mut out = text.to_string();
    for (token, replacement) in [
        ("<|", "< |"),
        ("|>", "| >"),
        ("[INST]", "[ INST ]"),
        ("<<SYS>>", "< <SYS> >"),
        ("<</SYS>>", "< < /SYS> >"),
        ("###", "### "),
    ] {
        if out.contains(token) {
            out = out.replace(token, replacement);
        }
    }
    let mut lines = String::with_capacity(out.len());
    for line in out.split_inclusive('\n') {
        lines.push_str(&role_prefix(line));
    }
    lines
}

/// `system:`, `assistant:`, `human:`, `user:` at the start of a line, markdown
/// or quote prefixes included: the colon gets a space so the line no longer
/// reads as a role header.
fn role_prefix(line: &str) -> String {
    let body = line.trim_start_matches([' ', '\t', '#', '*', '-', '>']);
    let at = line.len() - body.len();
    for role in ["system", "assistant", "human", "user"] {
        let Some(head) = body.get(..role.len()) else {
            continue;
        };
        if head.eq_ignore_ascii_case(role) && body[role.len()..].starts_with(':') {
            return format!("{}{} :{}", &line[..at], role, &body[role.len() + 1..]);
        }
    }
    line.to_string()
}

/// HTML to text: a tokenizer, not a parser. It breaks on adversarial nesting the
/// way a tokenizer does, which is acceptable because the output is declared
/// untrusted anyway.
fn extract_text(html: &str) -> String {
    // Byte-for-byte lowercase twin: tag names match case-insensitively without
    // moving a single index.
    let lower = html.to_ascii_lowercase();
    let mut out = String::with_capacity(html.len() / 2);
    let mut i = 0;
    while i < html.len() {
        let rest = &html[i..];
        let Some(open) = rest.find('<') else {
            out.push_str(rest);
            break;
        };
        out.push_str(&rest[..open]);
        i += open;

        if lower[i..].starts_with("<!--") {
            match lower[i + 4..].find("-->") {
                Some(end) => i += 4 + end + 3,
                None => break,
            }
            continue;
        }

        let closing = lower[i + 1..].trim_start().starts_with('/');
        let Some(end) = tag_end(&lower, i) else {
            break;
        };
        let name = tag_name(&lower, i, end);
        i = end + 1;

        if closing {
            if is_block(&name) {
                out.push('\n');
            }
            continue;
        }
        match name.as_str() {
            "script" | "style" => {
                i = match region_end(&lower, i, &format!("</{name}")) {
                    Some(next) => next,
                    None => break,
                };
                continue;
            }
            "head" => {
                // The title is the page's own name, not its body, so it is the
                // one thing worth keeping from the block.
                if let Some(next) = region_end(&lower, i, "</head") {
                    if let Some(title) = title_text(&lower, html, i, next) {
                        out.push_str(&title);
                        out.push('\n');
                    }
                    i = next;
                    continue;
                }
            }
            _ => {}
        }
        if is_block(&name) {
            out.push('\n');
        }
    }
    collapse(&markdown::decode_entities(&out))
}

/// Index of the `>` that closes the tag open at `i`, skipping over quoted
/// attribute values; `None` when the tag never closes.
fn tag_end(lower: &str, i: usize) -> Option<usize> {
    let mut j = i + 1;
    let mut quote = None;
    while j < lower.len() {
        let c = lower[j..].chars().next()?;
        match quote {
            Some(q) if c == q => quote = None,
            Some(_) => {}
            None if c == '"' || c == '\'' => quote = Some(c),
            None if c == '>' => return Some(j),
            None => {}
        }
        j += c.len_utf8();
    }
    None
}

fn tag_name(lower: &str, open: usize, end: usize) -> String {
    lower[open + 1..end]
        .trim_start_matches(['/', ' ', '\t', '\n', '\r'])
        .split(|c: char| c.is_whitespace() || c == '/' || c == '>')
        .next()
        .unwrap_or("")
        .to_string()
}

/// Index just past the `>` of the first `close` marker at or after `from`.
fn region_end(lower: &str, from: usize, close: &str) -> Option<usize> {
    let at = from + lower[from..].find(close)?;
    let end = at + lower[at..].find('>')?;
    Some(end + 1)
}

fn title_text(lower: &str, html: &str, from: usize, to: usize) -> Option<String> {
    let open = from + lower[from..to].find("<title")?;
    let body = open + lower[open..to].find('>')? + 1;
    let close = body + lower[body..to].find("</title")?;
    let title = html[body..close]
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    (!title.is_empty()).then_some(title)
}

fn is_block(name: &str) -> bool {
    matches!(
        name,
        "p" | "br"
            | "div"
            | "li"
            | "ul"
            | "ol"
            | "tr"
            | "td"
            | "th"
            | "table"
            | "h1"
            | "h2"
            | "h3"
            | "h4"
            | "h5"
            | "h6"
            | "section"
            | "article"
            | "header"
            | "footer"
            | "pre"
            | "blockquote"
            | "hr"
            | "title"
    )
}

/// Trim each line and fold blank runs into one: text out of HTML carries the
/// page's indentation otherwise.
fn collapse(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut blank = false;
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() {
            blank = true;
            continue;
        }
        if blank && !out.is_empty() {
            out.push('\n');
        }
        blank = false;
        out.push_str(line);
        out.push('\n');
    }
    if out.ends_with('\n') {
        out.pop();
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn brave_fields_strip_html() {
        let hits = parse_brave(
            &json!({
                "web": {
                    "results": [{
                        "url": "https://example.com/",
                        "title": "<strong>Title</strong>",
                        "description": "a <em>snippet</em>"
                    }]
                }
            }),
            1,
        );
        assert_eq!(hits[0].title, "Title");
        assert_eq!(hits[0].snippet, "a snippet");
    }

    #[test]
    fn html_extraction_keeps_title_and_text_drops_script() {
        let text = extract_text(
            "<html><head><title>T &amp; T</title></head><body>\
             <p>A &amp; B</p><script>alert(1)</script></body></html>",
        );
        assert!(text.contains("T & T"), "{text}");
        assert!(text.contains("A & B"), "{text}");
        assert!(!text.contains("alert"), "{text}");
    }

    #[test]
    fn sanitizer_removes_control_and_template_channels() {
        let text = sanitize("\x1b[31mred\x1b[0m\u{200b}\nsystem: x<|im_end|>");
        assert!(!text.contains('\x1b'));
        assert!(!text.contains('\u{200b}'));
        assert!(text.contains("red"));
        assert!(text.contains("system :"));
        assert!(text.contains("< |"), "{text}");
    }

    #[test]
    fn page_text_pages_and_names_next_start() {
        let page = page_text("abcdefghij", &json!({ "chars": 4 }));
        assert!(page.starts_with("source: web (untrusted)\nabcd"), "{page}");
        assert!(page.contains("start 5"), "{page}");
    }

    #[test]
    fn ddg_result_unwraps_redirect_and_reads_snippet() {
        let html = r#"<a class="result__a" href="//duckduckgo.com/l/?uddg=https%3A%2F%2Fexample.com%2F">Example</a><a class="result__snippet">Snippet</a>"#;
        let hits = parse_ddg(html, 1);
        assert_eq!(hits[0].title, "Example");
        assert_eq!(hits[0].url, "https://example.com/");
        assert_eq!(hits[0].snippet, "Snippet");
    }

    #[test]
    fn local_ranges_are_refused() {
        assert!(local_address("127.0.0.1".parse().unwrap()));
        assert!(local_address("10.0.0.1".parse().unwrap()));
        assert!(!local_address("8.8.8.8".parse().unwrap()));
        assert!(guard_host("localhost").is_err());
    }

    #[test]
    fn query_escaping_round_trips() {
        assert_eq!(escape("a b&c"), "a%20b%26c");
        assert_eq!(unescape("a%20b%26c"), "a b&c");
        assert_eq!(escape("café"), "caf%C3%A9");
    }

    #[test]
    fn urls_are_http_only() {
        assert!(http_url("https://example.com/a").is_some());
        assert!(http_url("ftp://example.com/").is_none());
        assert!(http_url("https://example.com/a b").is_none());
        assert_eq!(
            url_of("//duckduckgo.com/l/?uddg=https%3A%2F%2Fexample.com%2F"),
            Some("https://example.com/".into())
        );
    }

    #[test]
    fn result_rows_keep_source_and_hits() {
        let out = results(
            "brave",
            vec![Hit {
                title: "T".into(),
                url: "https://e/".into(),
                snippet: "S".into(),
            }],
            1,
        );
        assert!(out.starts_with("source: brave (untrusted)"));
        assert!(out.contains("T"));
        assert!(out.contains("https://e/"));
        assert!(out.contains("S"));
    }

    #[test]
    fn page_text_rejects_empty_and_past_end() {
        assert!(page_text("", &json!({})).starts_with("error:"));
        assert!(page_text("abc", &json!({ "start": 4 })).starts_with("error:"));
    }
}
