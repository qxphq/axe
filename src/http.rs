use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use rustls::pki_types::ServerName;
use rustls::{ClientConfig, ClientConnection, RootCertStore, StreamOwned};

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

pub fn parse_url(url: &str) -> Result<Endpoint, String> {
    let (scheme, rest) = if let Some(rest) = url.strip_prefix("http://") {
        (Scheme::Http, rest)
    } else if let Some(rest) = url.strip_prefix("https://") {
        (Scheme::Https, rest)
    } else {
        return Err(format!("base_url must start with http:// or https://: {url}"));
    };

    let (authority, path) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, ""),
    };
    if authority.is_empty() {
        return Err(format!("missing host in {url}"));
    }

    let default_port = match scheme {
        Scheme::Http => 80,
        Scheme::Https => 443,
    };
    let (host, port) = match authority.rsplit_once(':') {
        Some((h, p)) => (
            h.to_string(),
            p.parse::<u16>()
                .map_err(|_| format!("bad port in {url}"))?,
        ),
        None => (authority.to_string(), default_port),
    };

    Ok(Endpoint {
        scheme,
        host,
        port,
        prefix: path.trim_end_matches('/').to_string(),
    })
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

fn connect(ep: &Endpoint) -> Result<Stream, String> {
    let stream = TcpStream::connect((ep.host.as_str(), ep.port))
        .map_err(|e| format!("connect {}:{}: {e}", ep.host, ep.port))?;
    stream
        .set_read_timeout(Some(Duration::from_secs(300)))
        .map_err(|e| format!("set timeout: {e}"))?;
    let _ = stream.set_nodelay(true);

    match ep.scheme {
        Scheme::Http => Ok(Stream::Plain(stream)),
        Scheme::Https => {
            let name = ServerName::try_from(ep.host.clone())
                .map_err(|e| format!("bad hostname {}: {e}", ep.host))?;
            let conn = ClientConnection::new(tls_config()?, name)
                .map_err(|e| format!("tls: {e}"))?;
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
        loop {
            match &mut self.state {
                BodyState::Eof => return self.inner.read(buf),
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
                    return Ok(n);
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
                    return Ok(n);
                }
            }
        }
    }
}

pub struct Response {
    pub status: u16,
    reader: BufReader<BodyReader<BufReader<Stream>>>,
}

impl Response {
    pub fn read_all(&mut self) -> io::Result<String> {
        let mut out = String::new();
        self.reader.read_to_string(&mut out)?;
        Ok(out)
    }

    pub fn read_line(&mut self) -> io::Result<Option<String>> {
        let mut line = String::new();
        let n = self.reader.read_line(&mut line)?;
        if n == 0 {
            Ok(None)
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
) -> Result<Response, String> {
    let mut stream = connect(ep)?;

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
    loop {
        let line = read_line(&mut reader)?;
        let trimmed = line.trim_end_matches(|c| c == '\r' || c == '\n');
        if trimmed.is_empty() {
            break;
        }
        if let Some((name, value)) = trimmed.split_once(':') {
            let name = name.trim().to_ascii_lowercase();
            let value = value.trim();
            if name == "content-length" {
                content_length = value.parse::<u64>().ok();
            } else if name == "transfer-encoding"
                && value.to_ascii_lowercase().contains("chunked")
            {
                chunked = true;
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
        reader: BufReader::new(BodyReader {
            inner: reader,
            state,
        }),
    })
}

fn read_line<R: BufRead>(reader: &mut R) -> Result<String, String> {
    let mut line = String::new();
    let n = reader.read_line(&mut line).map_err(|e| format!("read: {e}"))?;
    if n == 0 {
        return Err("connection closed before response".into());
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
    }
    Ok(String::from_utf8_lossy(&out).into_owned())
}
