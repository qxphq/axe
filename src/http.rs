//! URL parsing and HTTP/1.1 over `TcpStream`/rustls, with body decoding for
//! `Content-Length`, chunked and close-delimited responses.

use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::{Shutdown, SocketAddr, TcpStream, ToSocketAddrs};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use rustls::pki_types::ServerName;
use rustls::{ClientConfig, ClientConnection, RootCertStore, StreamOwned};

/// One line of a status/header/SSE stream. A server must not grow the parent's
/// memory with an unterminated line.
const LINE_MAX: u64 = 1 << 20;
/// Whole-body cap for model lists and error bodies.
const BODY_MAX: u64 = 8 << 20;
const CONNECT_SECONDS: u64 = 10;
const WRITE_SECONDS: u64 = 30;

// ---- the target ----

#[derive(Clone)]
enum Scheme {
    Http,
    Https,
}

#[derive(Clone)]
pub struct Endpoint {
    scheme: Scheme,
    pub host: String,
    pub port: u16,
    pub prefix: String,
}

/// Stops an in-flight request: the socket is shut down so a blocked read
/// returns at once, and the flag lets the turn loop stop between requests. A
/// turn owns one, so a stale request can never stop the next turn.
pub struct Cancel {
    flag: AtomicBool,
    socket: Mutex<Option<TcpStream>>,
}

impl Cancel {
    pub fn new() -> Cancel {
        Cancel {
            flag: AtomicBool::new(false),
            socket: Mutex::new(None),
        }
    }

    pub fn cancelled(&self) -> bool {
        self.flag.load(Ordering::Relaxed)
    }

    /// Raise the flag and unblock the request in flight, if there is one.
    pub fn cancel(&self) {
        self.flag.store(true, Ordering::SeqCst);
        let socket = self.socket.lock().ok().and_then(|mut slot| slot.take());
        if let Some(socket) = socket {
            let _ = socket.shutdown(Shutdown::Both);
        }
    }

    /// The request in flight hands over a handle it can be closed with. The
    /// previous request is finished or abandoned, so it is replaced. The flag
    /// is read under the same lock `cancel` takes, so an interrupt that lands
    /// between the connect and this call is not lost.
    fn watch(&self, socket: &TcpStream) {
        let Ok(clone) = socket.try_clone() else {
            return;
        };
        let Ok(mut slot) = self.socket.lock() else {
            return;
        };
        if self.cancelled() {
            let _ = clone.shutdown(Shutdown::Both);
        } else {
            *slot = Some(clone);
        }
    }
}

pub fn parse_url(url: &str) -> Result<Endpoint, String> {
    split_url(url).map(|(ep, _)| ep)
}

/// Split a URL into its endpoint and the exact request target. `prefix` keeps
/// the provider meaning — the base path with the trailing slash trimmed, empty
/// when there is none — while the target is the path and query as written, so a
/// page URL reaches the server intact.
pub fn split_url(url: &str) -> Result<(Endpoint, String), String> {
    // The target is written to the socket as given: a control byte or a space
    // in it would split the request line.
    if let Some(c) = url.chars().find(|c| c.is_ascii_control() || *c == ' ') {
        return Err(format!("invalid character {c:?} in URL: {url}"));
    }
    let (scheme, rest) = if let Some(rest) = url.strip_prefix("http://") {
        (Scheme::Http, rest)
    } else if let Some(rest) = url.strip_prefix("https://") {
        (Scheme::Https, rest)
    } else {
        return Err(format!(
            "base_url must start with http:// or https://: {url}"
        ));
    };

    // The authority ends at the first '/', '?' or '#': a query can follow a host
    // with no path at all.
    let end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let (authority, tail) = rest.split_at(end);
    if authority.is_empty() {
        return Err(format!("missing host in {url}"));
    }
    let target = if tail.is_empty() {
        "/".to_string()
    } else if tail.starts_with('/') {
        tail.to_string()
    } else {
        format!("/{tail}")
    };
    let target = match target.find('#') {
        Some(i) => target[..i].to_string(),
        None => target,
    };
    // `target` is fragment-free; using it keeps a `#` out of the endpoint's
    // base path (a query on a `models_url` is kept).
    let path = if tail.starts_with('/') {
        target.as_str()
    } else {
        ""
    };

    let default_port = match scheme {
        Scheme::Http => 80,
        Scheme::Https => 443,
    };
    let (host, port) = match authority.rsplit_once(':') {
        Some((h, p)) => (
            h.to_string(),
            p.parse::<u16>().map_err(|_| format!("bad port in {url}"))?,
        ),
        None => (authority.to_string(), default_port),
    };

    Ok((
        Endpoint {
            scheme,
            host,
            port,
            prefix: path.trim_end_matches('/').to_string(),
        },
        target,
    ))
}

/// An RFC 9110 token: what may name a header field.
pub(crate) fn valid_header_name(name: &str) -> bool {
    valid_name(name)
}

/// An RFC 9110 token: what may name a header field.
fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&b))
}

/// A header value may hold spaces and visible text, but no control byte:
/// CR/LF would end the field and start another.
fn valid_value(value: &str) -> bool {
    !value.chars().any(|c| c.is_ascii_control())
}

/// `Host` names the port only when it is not the scheme's default.
fn host_header(ep: &Endpoint) -> String {
    let default = match ep.scheme {
        Scheme::Http => 80,
        Scheme::Https => 443,
    };
    if ep.port == default {
        ep.host.clone()
    } else {
        format!("{}:{}", ep.host, ep.port)
    }
}

// ---- the connection ----

enum Stream {
    Plain(TcpStream),
    Tls(Box<StreamOwned<ClientConnection, TcpStream>>),
}

impl Read for Stream {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        match self {
            Stream::Plain(s) => s.read(buf),
            Stream::Tls(s) => s.read(buf),
        }
    }
}

impl Write for Stream {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        match self {
            Stream::Plain(s) => s.write(buf),
            Stream::Tls(s) => s.write(buf),
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        match self {
            Stream::Plain(s) => s.flush(),
            Stream::Tls(s) => s.flush(),
        }
    }
}

static TLS: OnceLock<Arc<ClientConfig>> = OnceLock::new();

fn tls_config() -> Result<Arc<ClientConfig>, String> {
    if let Some(config) = TLS.get() {
        return Ok(config.clone());
    }
    let roots = RootCertStore {
        roots: webpki_roots::TLS_SERVER_ROOTS.to_vec(),
    };
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let config = ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .map_err(|e| format!("tls: {e}"))?
        .with_root_certificates(roots)
        .with_no_client_auth();
    let config = Arc::new(config);
    let _ = TLS.set(config.clone());
    Ok(config)
}

fn connect(
    ep: &Endpoint,
    pinned: Option<&[SocketAddr]>,
    cancel: Option<&Cancel>,
    read_timeout: Duration,
) -> Result<Stream, String> {
    if cancel.is_some_and(Cancel::cancelled) {
        return Err("interrupted".into());
    }
    // A caller that already resolved and vetted the name passes its addresses
    // here, so the name is not resolved twice: DNS cannot change between the
    // check and the connect. Everything else resolves as usual.
    let addrs: Vec<SocketAddr> = match pinned {
        Some(addrs) => addrs.to_vec(),
        None => (ep.host.as_str(), ep.port)
            .to_socket_addrs()
            .map_err(|e| format!("resolve {}:{}: {e}", ep.host, ep.port))?
            .collect(),
    };
    let mut stream = None;
    let mut last: Option<io::Error> = None;
    for addr in addrs {
        match TcpStream::connect_timeout(&addr, Duration::from_secs(CONNECT_SECONDS)) {
            Ok(s) => {
                stream = Some(s);
                break;
            }
            Err(e) => last = Some(e),
        }
    }
    let stream = stream.ok_or_else(|| {
        let why = last
            .map(|e| e.to_string())
            .unwrap_or_else(|| "no addresses".to_string());
        format!("connect {}:{}: {why}", ep.host, ep.port)
    })?;
    stream
        .set_read_timeout(Some(read_timeout))
        .map_err(|e| format!("set timeout: {e}"))?;
    stream
        .set_write_timeout(Some(Duration::from_secs(WRITE_SECONDS)))
        .map_err(|e| format!("set timeout: {e}"))?;
    let _ = stream.set_nodelay(true);
    if let Some(cancel) = cancel {
        cancel.watch(&stream);
    }

    match ep.scheme {
        Scheme::Http => Ok(Stream::Plain(stream)),
        Scheme::Https => {
            let name = ServerName::try_from(ep.host.clone())
                .map_err(|e| format!("bad hostname {}: {e}", ep.host))?;
            let conn =
                ClientConnection::new(tls_config()?, name).map_err(|e| format!("tls: {e}"))?;
            Ok(Stream::Tls(Box::new(StreamOwned::new(conn, stream))))
        }
    }
}

enum BodyState {
    Length(u64),
    Chunked { remaining: usize, done: bool },
    Eof,
}

struct BodyReader<R: Read> {
    inner: R,
    state: BodyState,
}

impl<R: Read> Read for BodyReader<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        match &mut self.state {
            BodyState::Eof => self.inner.read(buf),
            BodyState::Length(remaining) => {
                if *remaining == 0 {
                    return Ok(0);
                }
                let max = (*remaining).min(buf.len() as u64) as usize;
                let n = self.inner.read(&mut buf[..max])?;
                if n == 0 {
                    *remaining = 0;
                    return Ok(0);
                }
                *remaining -= n as u64;
                Ok(n)
            }
            BodyState::Chunked { remaining, done } => {
                if *done {
                    return Ok(0);
                }
                if *remaining == 0 {
                    let line = read_line_raw(&mut self.inner)?;
                    let size = line.trim().split(';').next().unwrap_or("").trim();
                    if size.is_empty() {
                        return Ok(0);
                    }
                    let size = usize::from_str_radix(size, 16).map_err(|_| {
                        io::Error::new(
                            io::ErrorKind::InvalidData,
                            format!("bad chunk size: {line:?}"),
                        )
                    })?;
                    if size == 0 {
                        loop {
                            let trailer = read_line_raw(&mut self.inner)?;
                            if trailer.trim().is_empty() {
                                break;
                            }
                        }
                        *done = true;
                        return Ok(0);
                    }
                    *remaining = size;
                }
                let max = (*remaining).min(buf.len());
                let n = self.inner.read(&mut buf[..max])?;
                if n == 0 {
                    return Err(io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        "truncated chunk",
                    ));
                }
                *remaining -= n;
                if *remaining == 0 {
                    let mut crlf = [0u8; 2];
                    self.inner.read_exact(&mut crlf)?;
                }
                Ok(n)
            }
        }
    }
}

// ---- the response ----

pub struct Response {
    pub status: u16,
    pub content_type: Option<String>,
    pub location: Option<String>,
    /// Every response header, names lowercased. Transport details such as
    /// `Mcp-Session-Id` live here.
    pub headers: Vec<(String, String)>,
    reader: BufReader<BodyReader<BufReader<Stream>>>,
}

impl Response {
    pub fn read_all(&mut self) -> io::Result<String> {
        let mut out = String::new();
        self.reader
            .by_ref()
            .take(BODY_MAX)
            .read_to_string(&mut out)?;
        Ok(out)
    }

    /// Read at most `limit` bytes, then stop: the cap is on bytes off the wire.
    /// Decoded lossily, so a page that is not UTF-8 is mojibake rather than an
    /// error. `deadline` bounds the loop, not a single blocked read.
    pub fn read_capped(&mut self, limit: u64, deadline: Instant) -> io::Result<String> {
        let mut out = Vec::new();
        let mut chunk = [0u8; 8192];
        while (out.len() as u64) < limit {
            if Instant::now() > deadline {
                break;
            }
            let room = (limit - out.len() as u64).min(chunk.len() as u64) as usize;
            let n = self.reader.read(&mut chunk[..room])?;
            if n == 0 {
                break;
            }
            out.extend_from_slice(&chunk[..n]);
        }
        Ok(String::from_utf8_lossy(&out).into_owned())
    }

    pub fn read_line(&mut self) -> io::Result<Option<String>> {
        let mut line = String::new();
        let n = self.reader.by_ref().take(LINE_MAX).read_line(&mut line)?;
        if n == 0 {
            Ok(None)
        } else if !line.ends_with('\n') && n as u64 >= LINE_MAX {
            Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "stream line too long",
            ))
        } else {
            Ok(Some(line))
        }
    }
}

// ---- requests ----

pub fn request(
    ep: &Endpoint,
    method: &str,
    path: &str,
    api_key: Option<&str>,
    headers: &[(String, String)],
    body: Option<&str>,
    cancel: Option<&Cancel>,
) -> Result<Response, String> {
    request_with_timeout(
        ep,
        method,
        path,
        api_key,
        headers,
        body,
        None,
        cancel,
        Duration::from_secs(300),
    )
}

/// Like `request`, with the socket read timeout chosen by the caller. The body
/// reader still has its own deadline; this bounds a single blocked read.
#[allow(clippy::too_many_arguments)]
pub fn request_with_timeout(
    ep: &Endpoint,
    method: &str,
    path: &str,
    api_key: Option<&str>,
    headers: &[(String, String)],
    body: Option<&str>,
    pinned: Option<&[SocketAddr]>,
    cancel: Option<&Cancel>,
    read_timeout: Duration,
) -> Result<Response, String> {
    let target = format!("{}{}", ep.prefix, path);
    // Every byte reaches the socket as given; a space or a control byte would
    // end the request line early.
    if let Some(c) = target.chars().find(|c| c.is_ascii_control() || *c == ' ') {
        return Err(format!("invalid character {c:?} in request target"));
    }
    if let Some(key) = api_key {
        if !valid_value(key) {
            return Err("invalid value for the Authorization header".into());
        }
    }
    for (name, value) in headers {
        if !valid_name(name) {
            return Err(format!("invalid header name: {name:?}"));
        }
        if !valid_value(value) {
            return Err(format!("invalid value for header {name}"));
        }
    }

    let mut stream = connect(ep, pinned, cancel, read_timeout)?;

    let mut head = format!(
        "{method} {target} HTTP/1.1\r\nHost: {}\r\nUser-Agent: axe/{}\r\nConnection: close\r\n",
        host_header(ep),
        env!("CARGO_PKG_VERSION")
    );
    // A caller that names an Accept (MCP's streamable HTTP needs
    // `application/json, text/event-stream`) keeps it; a second `Accept: */*`
    // read first would fail a strict server like Home Assistant.
    if !headers
        .iter()
        .any(|(name, _)| name.eq_ignore_ascii_case("accept"))
    {
        head.push_str("Accept: */*\r\n");
    }
    if let Some(key) = api_key {
        head.push_str(&format!("Authorization: Bearer {key}\r\n"));
    }
    for (name, value) in headers {
        head.push_str(&format!("{name}: {value}\r\n"));
    }
    if let Some(b) = body {
        // A caller that sets its own type keeps it.
        if !headers
            .iter()
            .any(|(name, _)| name.eq_ignore_ascii_case("content-type"))
        {
            head.push_str("Content-Type: application/json\r\n");
        }
        head.push_str(&format!("Content-Length: {}\r\n", b.len()));
    }
    head.push_str("\r\n");

    stream
        .write_all(head.as_bytes())
        .and_then(|_| match body {
            Some(b) => stream.write_all(b.as_bytes()),
            None => Ok(()),
        })
        .map_err(|e| format!("write: {e}"))?;
    let _ = stream.flush();

    let mut reader = BufReader::new(stream);

    let status_line = read_line(&mut reader)?;
    let mut parts = status_line.split_whitespace();
    let _version = parts.next();
    let status = parts
        .next()
        .and_then(|s| s.parse::<u16>().ok())
        .ok_or_else(|| format!("bad status line: {status_line:?}"))?;

    let mut content_length = None;
    let mut chunked = false;
    let mut content_type = None;
    let mut location = None;
    let mut headers_out: Vec<(String, String)> = Vec::new();
    loop {
        let line = read_line(&mut reader)?;
        let trimmed = line.trim_end_matches(['\r', '\n']);
        if trimmed.is_empty() {
            break;
        }
        if let Some((name, value)) = trimmed.split_once(':') {
            let name = name.trim().to_ascii_lowercase();
            let value = value.trim();
            headers_out.push((name.clone(), value.to_string()));
            if name == "content-length" {
                content_length = value.parse::<u64>().ok();
            } else if name == "transfer-encoding" && value.to_ascii_lowercase().contains("chunked")
            {
                chunked = true;
            } else if name == "content-type" {
                content_type.get_or_insert_with(|| value.to_string());
            } else if name == "location" {
                location.get_or_insert_with(|| value.to_string());
            }
        }
    }

    let state = if chunked {
        BodyState::Chunked {
            remaining: 0,
            done: false,
        }
    } else if let Some(n) = content_length {
        BodyState::Length(n)
    } else {
        BodyState::Eof
    };

    Ok(Response {
        status,
        content_type,
        location,
        headers: headers_out,
        reader: BufReader::new(BodyReader {
            inner: reader,
            state,
        }),
    })
}

// ---- lines ----

fn read_line<R: BufRead>(reader: &mut R) -> Result<String, String> {
    let mut line = String::new();
    let n = reader
        .by_ref()
        .take(LINE_MAX)
        .read_line(&mut line)
        .map_err(|e| format!("read: {e}"))?;
    if n == 0 {
        return Err("connection closed before response".into());
    }
    if !line.ends_with('\n') && n as u64 >= LINE_MAX {
        return Err("response line too long".into());
    }
    Ok(line)
}

fn read_line_raw<R: Read>(reader: &mut R) -> io::Result<String> {
    let mut out = Vec::new();
    let mut byte = [0u8; 1];
    loop {
        let n = reader.read(&mut byte)?;
        if n == 0 {
            break;
        }
        out.push(byte[0]);
        if byte[0] == b'\n' {
            break;
        }
        if out.len() as u64 >= LINE_MAX {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "line too long"));
        }
    }
    Ok(String::from_utf8_lossy(&out).into_owned())
}

/// A loopback HTTP server for the tests that need one. It answers the
/// connections it is given, in order, with the exact bytes it is given, and
/// reports what the client wrote, so a test asserts on the wire and not on a
/// mock's idea of it. Plain HTTP only: TLS would need a certificate this
/// repository does not ship.
#[cfg(test)]
pub(crate) mod support {
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::sync::mpsc::{self, Receiver};
    use std::thread::{self, JoinHandle};
    use std::time::Duration;

    use super::{parse_url, Endpoint};

    pub struct Server {
        pub ep: Endpoint,
        got: Receiver<Vec<u8>>,
        /// Kept only so the thread outlives the last connection it serves; it
        /// is never joined, because a test may leave a response unasked.
        _thread: JoinHandle<()>,
    }

    impl Server {
        /// One response per connection, in order, on a port the kernel picks.
        pub fn new(responses: Vec<Vec<u8>>) -> Server {
            let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback");
            let port = listener.local_addr().expect("loopback addr").port();
            let (tx, got) = mpsc::channel();
            let thread = thread::spawn(move || {
                for response in &responses {
                    let Ok((mut stream, _)) = listener.accept() else {
                        return;
                    };
                    let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
                    let _ = tx.send(read_request(&mut stream));
                    let _ = stream.write_all(response);
                    let _ = stream.flush();
                }
            });
            let url = format!("http://127.0.0.1:{port}");
            Server {
                ep: parse_url(&url).expect("loopback url"),
                got,
                _thread: thread,
            }
        }

        /// The next request as the client wrote it, headers and body.
        pub fn text(&self) -> String {
            String::from_utf8_lossy(&self.got.recv().expect("request")).into_owned()
        }
    }

    fn read_request(stream: &mut std::net::TcpStream) -> Vec<u8> {
        let mut buf = Vec::new();
        let mut chunk = [0u8; 4096];
        loop {
            match stream.read(&mut chunk) {
                Ok(0) | Err(_) => break,
                Ok(n) => buf.extend_from_slice(&chunk[..n]),
            }
            let Some(end) = buf.windows(4).position(|w| w == b"\r\n\r\n") else {
                continue;
            };
            // Every request this client writes carries a length, so the body
            // needs no framing guesswork.
            let head = String::from_utf8_lossy(&buf[..end]).to_ascii_lowercase();
            let length = head
                .lines()
                .find_map(|line| line.strip_prefix("content-length:"))
                .and_then(|value| value.trim().parse::<usize>().ok())
                .unwrap_or(0);
            if buf.len() >= end + 4 + length {
                break;
            }
        }
        buf
    }

    /// A response with a length, the headers given, and a body.
    pub fn reply(status: &str, headers: &[(&str, &str)], body: &str) -> Vec<u8> {
        let mut out = format!("HTTP/1.1 {status}\r\n");
        for (name, value) in headers {
            out.push_str(&format!("{name}: {value}\r\n"));
        }
        out.push_str(&format!("Content-Length: {}\r\n", body.len()));
        out.push_str("Connection: close\r\n\r\n");
        out.push_str(body);
        out.into_bytes()
    }

    /// A 200 `application/json` with a length.
    pub fn json(body: &str) -> Vec<u8> {
        reply("200 OK", &[("Content-Type", "application/json")], body)
    }

    /// A 200 body sent in chunks, so the reader has to decode the framing.
    pub fn chunked(body: &[&str]) -> Vec<u8> {
        let mut out = String::from(
            "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\n\
                          Transfer-Encoding: chunked\r\nConnection: close\r\n\r\n",
        );
        for piece in body {
            out.push_str(&format!("{:x}\r\n{piece}\r\n", piece.len()));
        }
        out.push_str("0\r\n\r\n");
        out.into_bytes()
    }

    /// A 200 body with no length at all: the connection closing is the end of
    /// it, which is what `llm.local` does.
    pub fn close_delimited(body: &str) -> Vec<u8> {
        format!("HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\n\r\n{body}").into_bytes()
    }

    /// One SSE `data:` line per entry, as the provider writes it.
    pub fn sse(events: &[&str]) -> Vec<u8> {
        let framed: Vec<String> = events.iter().map(|e| format!("data: {e}\n\n")).collect();
        let pieces: Vec<&str> = framed.iter().map(String::as_str).collect();
        chunked(&pieces)
    }
}

#[cfg(test)]
mod tests {
    use super::support::{chunked, close_delimited, json, reply, sse, Server};
    use super::*;

    #[test]
    fn split_keeps_base_path_and_target() {
        let (ep, target) = split_url("http://host:8080/v1/models?q=1#frag").unwrap();
        assert_eq!(ep.host, "host");
        assert_eq!(ep.port, 8080);
        assert_eq!(ep.prefix, "/v1/models?q=1".trim_end_matches('/'));
        assert_eq!(target, "/v1/models?q=1");
    }

    #[test]
    fn split_uses_default_port_and_root() {
        let (ep, target) = split_url("https://host").unwrap();
        assert_eq!(ep.port, 443);
        assert_eq!(target, "/");
    }

    #[test]
    fn urls_reject_controls_and_spaces() {
        for url in [
            "http://host/a\r\nX: y",
            "http://host/a\nb",
            "http://host/a b",
            "http://host/a\tb",
        ] {
            assert!(split_url(url).is_err(), "{url:?}");
        }
    }

    #[test]
    fn header_names_and_values_are_checked() {
        assert!(valid_name("x-opencode-session"));
        assert!(!valid_name(""));
        assert!(!valid_name("X: y"));
        assert!(valid_value("Bearer abc.DEF-123"));
        assert!(!valid_value("a\r\nX: y"));
        assert!(!valid_value("a\tb"));
    }

    #[test]
    fn host_carries_a_non_default_port() {
        let (ep, _) = split_url("http://host:8080/v1").unwrap();
        assert_eq!(host_header(&ep), "host:8080");
        let (ep, _) = split_url("https://host/v1").unwrap();
        assert_eq!(host_header(&ep), "host");
    }

    #[test]
    fn body_reads_length_and_chunked() {
        let mut fixed = BodyReader {
            inner: &b"abcdef"[..],
            state: BodyState::Length(3),
        };
        let mut out = String::new();
        fixed.read_to_string(&mut out).unwrap();
        assert_eq!(out, "abc");

        let mut chunked = BodyReader {
            inner: &b"3\r\nabc\r\n0\r\n\r\n"[..],
            state: BodyState::Chunked {
                remaining: 0,
                done: false,
            },
        };
        let mut out = String::new();
        chunked.read_to_string(&mut out).unwrap();
        assert_eq!(out, "abc");
    }

    #[test]
    fn raw_line_reads_until_newline() {
        let mut reader = &b"3\r\nrest"[..];
        assert_eq!(read_line_raw(&mut reader).unwrap(), "3\r\n");
        assert_eq!(read_line_raw(&mut reader).unwrap(), "rest");
    }

    #[test]
    fn split_rejects_bad_urls() {
        assert!(split_url("ftp://host").is_err());
        assert!(split_url("http://").is_err());
        assert!(split_url("http://host:bad/").is_err());
    }

    #[test]
    fn body_reports_truncation_and_bad_chunks() {
        let mut truncated = BodyReader {
            inner: &b"ab"[..],
            state: BodyState::Chunked {
                remaining: 3,
                done: false,
            },
        };
        let mut buf = [0u8; 4];
        assert_eq!(truncated.read(&mut buf).unwrap(), 2);
        assert!(truncated.read(&mut buf).is_err());

        let mut bad = BodyReader {
            inner: &b"zz\r\n"[..],
            state: BodyState::Chunked {
                remaining: 0,
                done: false,
            },
        };
        assert!(bad.read(&mut buf).is_err());
    }

    #[test]
    fn body_empty_and_eof() {
        let mut empty = BodyReader {
            inner: &b""[..],
            state: BodyState::Eof,
        };
        let mut buf = [0u8; 4];
        assert_eq!(empty.read(&mut buf).unwrap(), 0);
        let mut length_zero = BodyReader {
            inner: &b"x"[..],
            state: BodyState::Length(0),
        };
        assert_eq!(length_zero.read(&mut buf).unwrap(), 0);
    }

    #[test]
    fn cancel_is_sticky() {
        let cancel = Cancel::new();
        assert!(!cancel.cancelled());
        cancel.cancel();
        assert!(cancel.cancelled());
        cancel.cancel();
        assert!(cancel.cancelled());
    }

    #[test]
    fn header_line_reader_reports_eof() {
        let mut reader = &b"a: b\r\n"[..];
        assert_eq!(read_line(&mut reader).unwrap(), "a: b\r\n");
        assert!(read_line(&mut reader).is_err());
    }

    #[test]
    fn a_request_writes_the_target_key_and_headers() {
        let server = Server::new(vec![json(r#"{"ok":true}"#)]);
        let ep = &server.ep;
        let mut res = request(
            ep,
            "GET",
            "/v1/models?q=1",
            Some("secret"),
            &[("x-opencode-session".into(), "sid".into())],
            None,
            None,
        )
        .unwrap();
        assert_eq!(res.status, 200);
        assert_eq!(res.content_type.as_deref(), Some("application/json"));
        assert_eq!(res.read_all().unwrap(), r#"{"ok":true}"#);

        let sent = server.text();
        assert!(
            sent.starts_with("GET /v1/models?q=1 HTTP/1.1\r\n"),
            "{sent:?}"
        );
        assert!(sent.contains("\r\nHost: 127.0.0.1:"), "{sent:?}");
        assert!(
            sent.contains("\r\nAuthorization: Bearer secret\r\n"),
            "{sent:?}"
        );
        assert!(sent.contains("\r\nx-opencode-session: sid\r\n"), "{sent:?}");
        assert!(sent.ends_with("\r\n\r\n"), "{sent:?}");
    }

    #[test]
    fn a_post_sends_its_length_and_body() {
        let server = Server::new(vec![json("{}")]);
        let mut res = request(
            &server.ep,
            "POST",
            "/chat",
            None,
            &[],
            Some("{\"a\":1}"),
            None,
        )
        .unwrap();
        assert_eq!(res.read_all().unwrap(), "{}");
        let sent = server.text();
        assert!(sent.contains("\r\nContent-Length: 7\r\n"), "{sent:?}");
        assert!(sent.ends_with("{\"a\":1}"), "{sent:?}");
    }

    #[test]
    fn a_chunked_body_decodes_across_reads() {
        let server = Server::new(vec![chunked(&["hello ", "wor", "ld"])]);
        let mut res = request(&server.ep, "GET", "/", None, &[], None, None).unwrap();
        assert_eq!(res.read_all().unwrap(), "hello world");
    }

    #[test]
    fn a_body_without_a_length_ends_at_the_close() {
        let server = Server::new(vec![close_delimited("plain text")]);
        let mut res = request(&server.ep, "GET", "/", None, &[], None, None).unwrap();
        assert_eq!(res.read_all().unwrap(), "plain text");
    }

    #[test]
    fn a_reader_takes_lines_and_reports_the_end() {
        let server = Server::new(vec![sse(&["{\"a\":1}", "{\"b\":2}"])]);
        let mut res = request(&server.ep, "GET", "/", None, &[], None, None).unwrap();
        assert_eq!(
            res.read_line().unwrap().as_deref(),
            Some("data: {\"a\":1}\n")
        );
        assert_eq!(res.read_line().unwrap().as_deref(), Some("\n"));
        assert_eq!(
            res.read_line().unwrap().as_deref(),
            Some("data: {\"b\":2}\n")
        );
        assert_eq!(res.read_line().unwrap().as_deref(), Some("\n"));
        assert_eq!(res.read_line().unwrap(), None);
    }

    #[test]
    fn a_capped_read_stops_at_the_limit_or_the_deadline() {
        let server = Server::new(vec![json("abcdefghij")]);
        let mut res = request(&server.ep, "GET", "/", None, &[], None, None).unwrap();
        let cut = res
            .read_capped(4, Instant::now() + Duration::from_secs(5))
            .unwrap();
        assert_eq!(cut, "abcd");

        let server = Server::new(vec![json("abcdefghij")]);
        let mut res = request(&server.ep, "GET", "/", None, &[], None, None).unwrap();
        let expired = res.read_capped(4, Instant::now()).unwrap();
        assert_eq!(expired, "", "a deadline already past reads nothing");
    }

    #[test]
    fn the_location_and_type_headers_are_kept() {
        let server = Server::new(vec![reply(
            "302 Found",
            &[
                ("Location", "https://example.com/next"),
                ("Content-Type", "text/html"),
            ],
            "",
        )]);
        let mut res = request(&server.ep, "GET", "/", None, &[], None, None).unwrap();
        assert_eq!(res.status, 302);
        assert_eq!(res.location.as_deref(), Some("https://example.com/next"));
        assert_eq!(res.content_type.as_deref(), Some("text/html"));
        assert_eq!(res.read_all().unwrap(), "");
    }

    #[test]
    fn a_pinned_address_skips_the_name() {
        let server = Server::new(vec![json("{}")]);
        let addr: SocketAddr = ([127, 0, 0, 1], server.ep.port).into();
        let res = request_with_timeout(
            &server.ep,
            "GET",
            "/pinned",
            None,
            &[],
            None,
            Some(&[addr]),
            None,
            Duration::from_secs(5),
        );
        assert_eq!(res.unwrap().status, 200);
        assert!(server.text().starts_with("GET /pinned "));
    }

    #[test]
    fn a_cancelled_request_never_connects() {
        let cancel = Cancel::new();
        cancel.cancel();
        let server = Server::new(Vec::new());
        let err = request(&server.ep, "GET", "/", None, &[], None, Some(&cancel))
            .err()
            .unwrap();
        assert_eq!(err, "interrupted");
    }

    #[test]
    fn a_closed_socket_before_the_response_is_an_error() {
        let server = Server::new(vec![Vec::new()]);
        let err = request(&server.ep, "GET", "/", None, &[], None, None)
            .err()
            .unwrap();
        assert!(err.contains("closed before response"), "{err}");
    }

    #[test]
    fn a_bad_status_line_is_an_error() {
        let server = Server::new(vec![b"HTTP/1.1 banana\r\n\r\n".to_vec()]);
        let err = request(&server.ep, "GET", "/", None, &[], None, None)
            .err()
            .unwrap();
        assert!(err.contains("bad status line"), "{err}");
    }

    #[test]
    fn a_stalled_read_times_out() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            if let Ok((stream, _)) = listener.accept() {
                std::thread::sleep(Duration::from_millis(600));
                drop(stream);
            }
        });
        let ep = parse_url(&format!("http://127.0.0.1:{port}")).unwrap();
        let err = request_with_timeout(
            &ep,
            "GET",
            "/",
            None,
            &[],
            None,
            None,
            None,
            Duration::from_millis(100),
        )
        .err()
        .unwrap();
        assert!(err.starts_with("read:"), "{err}");
    }

    #[test]
    fn a_closed_port_is_reported_as_a_failed_connect() {
        // Bind, read the port, drop the listener: nothing is listening there.
        let port = {
            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            listener.local_addr().unwrap().port()
        };
        let ep = parse_url(&format!("http://127.0.0.1:{port}")).unwrap();
        let err = request(&ep, "GET", "/", None, &[], None, None)
            .err()
            .unwrap();
        assert!(
            err.starts_with(&format!("connect 127.0.0.1:{port}:")),
            "{err}"
        );
    }

    #[test]
    fn the_tls_config_is_built_once() {
        let first = tls_config().unwrap();
        let second = tls_config().unwrap();
        assert!(
            Arc::ptr_eq(&first, &second),
            "the certificate store is shared"
        );
    }

    #[test]
    fn a_header_that_would_split_the_request_is_refused() {
        let server = Server::new(Vec::new());
        let err = request(
            &server.ep,
            "GET",
            "/",
            Some("a\r\nX: y"),
            &[("bad name".into(), "v".into())],
            None,
            None,
        )
        .err()
        .unwrap();
        assert!(err.contains("Authorization"), "{err}");
        let err = request(
            &server.ep,
            "GET",
            "/",
            None,
            &[("bad name".into(), "v".into())],
            None,
            None,
        )
        .err()
        .unwrap();
        assert!(err.contains("invalid header name"), "{err}");
        let err = request(&server.ep, "GET", "/a b", None, &[], None, None)
            .err()
            .unwrap();
        assert!(err.contains("invalid character"), "{err}");
    }
}
