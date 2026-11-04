//! URL parsing and HTTP/1.1 over `TcpStream`/rustls, with body decoding for
//! `Content-Length`, chunked and close-delimited responses.

use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::{Shutdown, TcpStream, ToSocketAddrs};
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
    cancel: Option<&Cancel>,
    read_timeout: Duration,
) -> Result<Stream, String> {
    if cancel.is_some_and(Cancel::cancelled) {
        return Err("interrupted".into());
    }
    let addrs = (ep.host.as_str(), ep.port)
        .to_socket_addrs()
        .map_err(|e| format!("resolve {}:{}: {e}", ep.host, ep.port))?;
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

pub struct Response {
    pub status: u16,
    pub content_type: Option<String>,
    pub location: Option<String>,
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
    cancel: Option<&Cancel>,
    read_timeout: Duration,
) -> Result<Response, String> {
    let mut stream = connect(ep, cancel, read_timeout)?;

    let target = format!("{}{}", ep.prefix, path);
    let mut head = format!(
        "{method} {target} HTTP/1.1\r\nHost: {}\r\nUser-Agent: nano/{}\r\nAccept: */*\r\nConnection: close\r\n",
        ep.host,
        env!("CARGO_PKG_VERSION")
    );
    if let Some(key) = api_key {
        head.push_str(&format!("Authorization: Bearer {key}\r\n"));
    }
    for (name, value) in headers {
        head.push_str(&format!("{name}: {value}\r\n"));
    }
    if let Some(b) = body {
        head.push_str("Content-Type: application/json\r\n");
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
    loop {
        let line = read_line(&mut reader)?;
        let trimmed = line.trim_end_matches(['\r', '\n']);
        if trimmed.is_empty() {
            break;
        }
        if let Some((name, value)) = trimmed.split_once(':') {
            let name = name.trim().to_ascii_lowercase();
            let value = value.trim();
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
        reader: BufReader::new(BodyReader {
            inner: reader,
            state,
        }),
    })
}

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

#[cfg(test)]
mod tests {
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
}
