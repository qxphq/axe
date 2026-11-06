//! MCP OAuth: the sign-in a remote server asks for, and the tokens it leaves
//! behind. Discovery follows RFC 9728 and RFC 8414, registration is dynamic
//! (RFC 7591), the code flow uses PKCE S256 (RFC 7636), and the token store is
//! one owner-only file in the state tree.
//!
//! Nothing here runs in the background: a sign-in is `/mcp login`, which opens
//! a browser and waits on a loopback listener, and a request that meets a 401
//! refreshes with the stored token or fails with the command that fixes it.

use std::collections::BTreeMap;
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde_json::{json, Value};

use crate::http::{self, Cancel};
use crate::state;
use crate::text::strip_control;

/// How long a sign-in waits for the browser. A callback the user abandoned is
/// a timeout, not a stuck prompt: the REPL is blocked until it returns.
const CALLBACK_SECONDS: u64 = 180;
/// Refresh a little before the server would reject the token.
const SKEW: u64 = 30;

#[derive(Clone, Debug)]
pub struct Token {
    pub access: String,
    pub refresh: Option<String>,
    pub expires_at: Option<u64>,
    pub client_id: String,
    pub client_secret: Option<String>,
    pub token_endpoint: String,
}

impl Token {
    fn expired(&self) -> bool {
        self.expires_at.is_some_and(|at| at <= now() + SKEW)
    }

    fn value(&self) -> Value {
        json!({
            "access_token": self.access,
            "refresh_token": self.refresh,
            "expires_at": self.expires_at,
            "client_id": self.client_id,
            "client_secret": self.client_secret,
            "token_endpoint": self.token_endpoint,
        })
    }

    fn from_value(value: &Value) -> Option<Token> {
        Some(Token {
            access: value["access_token"].as_str()?.to_string(),
            refresh: value["refresh_token"].as_str().map(str::to_string),
            expires_at: value["expires_at"].as_u64(),
            client_id: value["client_id"].as_str()?.to_string(),
            client_secret: value["client_secret"].as_str().map(str::to_string),
            token_endpoint: value["token_endpoint"].as_str()?.to_string(),
        })
    }
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn auth_path() -> std::path::PathBuf {
    state::root().join("mcp-auth.json")
}

/// One server's state, keyed by name and URL: two entries pointing at the same
/// URL keep their own accounts.
fn key(name: &str, server: &str) -> String {
    format!("{name}|{server}")
}

fn read_store() -> BTreeMap<String, Value> {
    let Ok(raw) = fs::read_to_string(auth_path()) else {
        return BTreeMap::new();
    };
    let Ok(Value::Object(map)) = serde_json::from_str::<Value>(&raw) else {
        return BTreeMap::new();
    };
    map.into_iter().collect()
}

fn write_store(map: &BTreeMap<String, Value>) -> Result<(), String> {
    let path = auth_path();
    if let Some(dir) = path.parent() {
        state::private_dir(dir).map_err(|e| format!("mcp auth: {}: {e}", dir.display()))?;
    }
    let text = serde_json::to_string_pretty(map).map_err(|e| e.to_string())?;
    state::write_private(&path, &format!("{text}\n"))
        .map_err(|e| format!("mcp auth: {}: {e}", path.display()))
}

pub fn load(name: &str, server: &str) -> Option<Token> {
    read_store()
        .get(&key(name, server))
        .and_then(Token::from_value)
}

pub fn save(name: &str, server: &str, token: &Token) -> Result<(), String> {
    let mut map = read_store();
    map.insert(key(name, server), token.value());
    write_store(&map)
}

/// Whether a server has tokens, for `/mcp` and `/mcp logout`.
pub fn stored(name: &str, server: &str) -> bool {
    read_store().contains_key(&key(name, server))
}

pub fn remove(name: &str, server: &str) -> Result<bool, String> {
    let mut map = read_store();
    let removed = map.remove(&key(name, server)).is_some();
    if removed {
        write_store(&map)?;
    }
    Ok(removed)
}

/// The access token to send, refreshed first when it is about to expire.
pub fn access(name: &str, server: &str, cancel: &Cancel) -> Result<Option<String>, String> {
    let Some(token) = load(name, server) else {
        return Ok(None);
    };
    if !token.expired() {
        return Ok(Some(token.access));
    }
    let Some(_) = token.refresh else {
        return Ok(None);
    };
    let fresh = refresh(&token, cancel)?;
    save(name, server, &fresh)?;
    Ok(Some(fresh.access))
}

/// After a 401: exchange the refresh token for a new one. `None` when the
/// server has no refresh token, which is a sign-in, not an error.
pub fn reauthenticate(name: &str, server: &str, cancel: &Cancel) -> Result<Option<String>, String> {
    let Some(token) = load(name, server) else {
        return Ok(None);
    };
    let Some(_) = token.refresh else {
        return Ok(None);
    };
    let fresh = refresh(&token, cancel)?;
    save(name, server, &fresh)?;
    Ok(Some(fresh.access))
}

fn refresh(token: &Token, cancel: &Cancel) -> Result<Token, String> {
    let Some(refresh) = token.refresh.clone() else {
        return Err("no refresh token".into());
    };
    let mut form = vec![
        ("grant_type", "refresh_token".to_string()),
        ("refresh_token", refresh.clone()),
        ("client_id", token.client_id.clone()),
    ];
    if let Some(secret) = &token.client_secret {
        form.push(("client_secret", secret.clone()));
    }
    let answer = token_request(&token.token_endpoint, &form, cancel)?;
    Ok(Token {
        access: string(&answer, "access_token")?,
        refresh: answer["refresh_token"]
            .as_str()
            .map(str::to_string)
            .or(Some(refresh)),
        expires_at: answer["expires_in"].as_u64().map(|n| now() + n),
        client_id: token.client_id.clone(),
        client_secret: token.client_secret.clone(),
        token_endpoint: token.token_endpoint.clone(),
    })
}

/// A sign-in that has started: the URL to open, and the listener waiting for
/// the code. `begin` returns it so the caller can print the link before the
/// terminal blocks on the browser.
pub struct SignIn {
    pub url: String,
    flow: Flow,
}

struct Flow {
    name: String,
    server: String,
    token_endpoint: String,
    client_id: String,
    client_secret: Option<String>,
    verifier: String,
    state: String,
    redirect: String,
    listener: TcpListener,
}

/// Discovery, registration and a loopback listener: everything a sign-in needs
/// before the browser is asked. `open` is the browser (a no-op in tests it does
/// not control, and the caller prints the URL either way).
pub fn begin(name: &str, server: &str, open: &dyn Fn(&str)) -> Result<SignIn, String> {
    let (ep, _) = http::split_url(server)?;
    let origin = ep.origin();
    let metadata = protected_resource(&origin, &ep.prefix).unwrap_or(Value::Null);
    let issuer = metadata["authorization_servers"]
        .as_array()
        .and_then(|list| list.first())
        .and_then(Value::as_str)
        .map(str::to_string)
        .unwrap_or_else(|| origin.clone());
    let as_meta = authorization_server(&issuer)?;
    let authorization_endpoint = string(&as_meta, "authorization_endpoint")?;
    let token_endpoint = string(&as_meta, "token_endpoint")?;

    let listener = TcpListener::bind(("127.0.0.1", 0))
        .map_err(|e| format!("mcp auth: loopback listener: {e}"))?;
    listener.set_nonblocking(true).ok();
    let port = listener
        .local_addr()
        .map_err(|e| format!("mcp auth: loopback port: {e}"))?
        .port();
    let redirect = format!("http://127.0.0.1:{port}/callback");

    let (client_id, client_secret) = match stored_client(name, server) {
        Some(client) => client,
        None => register(&as_meta, &redirect)?,
    };
    let verifier = b64url(&random(32)?);
    let challenge = b64url(&sha256(verifier.as_bytes()));
    let state = b64url(&random(16)?);

    let url = format!(
        "{authorization_endpoint}{}response_type=code&client_id={}&redirect_uri={}&state={}\
         &code_challenge={challenge}&code_challenge_method=S256&resource={}",
        if authorization_endpoint.contains('?') {
            "&"
        } else {
            "?"
        },
        encode(&client_id),
        encode(&redirect),
        encode(&state),
        encode(server),
    );
    open(&url);
    Ok(SignIn {
        url,
        flow: Flow {
            name: name.to_string(),
            server: server.to_string(),
            token_endpoint,
            client_id,
            client_secret,
            verifier,
            state,
            redirect,
            listener,
        },
    })
}

impl SignIn {
    /// Wait for the browser to come back, exchange the code, and store what it
    /// bought. The listener is a loopback socket this process owns; anything
    /// that is not the callback is answered and ignored.
    pub fn finish(self, cancel: &Cancel) -> Result<Token, String> {
        let code = wait_for_code(&self.flow, cancel)?;
        let mut form = vec![
            ("grant_type", "authorization_code".to_string()),
            ("code", code),
            ("redirect_uri", self.flow.redirect.clone()),
            ("client_id", self.flow.client_id.clone()),
            ("code_verifier", self.flow.verifier.clone()),
            ("resource", self.flow.server.clone()),
        ];
        if let Some(secret) = &self.flow.client_secret {
            form.push(("client_secret", secret.clone()));
        }
        let answer = token_request(&self.flow.token_endpoint, &form, cancel)?;
        let token = Token {
            access: string(&answer, "access_token")?,
            refresh: answer["refresh_token"].as_str().map(str::to_string),
            expires_at: answer["expires_in"].as_u64().map(|n| now() + n),
            client_id: self.flow.client_id.clone(),
            client_secret: self.flow.client_secret.clone(),
            token_endpoint: self.flow.token_endpoint.clone(),
        };
        save(&self.flow.name, &self.flow.server, &token)?;
        Ok(token)
    }
}

/// The registration a previous sign-in made, if this server has one.
fn stored_client(name: &str, server: &str) -> Option<(String, Option<String>)> {
    let token = load(name, server)?;
    Some((token.client_id, token.client_secret))
}

fn protected_resource(origin: &str, path: &str) -> Result<Value, String> {
    let mut urls = vec![format!("{origin}/.well-known/oauth-protected-resource")];
    let path = path.trim_end_matches('/');
    if !path.is_empty() {
        urls.insert(
            0,
            format!("{origin}/.well-known/oauth-protected-resource{path}"),
        );
    }
    for url in urls {
        if let Ok(value) = get_json(&url) {
            return Ok(value);
        }
    }
    Err("no protected resource metadata".into())
}

fn authorization_server(issuer: &str) -> Result<Value, String> {
    let base = issuer.trim_end_matches('/');
    for url in [
        format!("{base}/.well-known/oauth-authorization-server"),
        format!("{base}/.well-known/openid-configuration"),
    ] {
        if let Ok(value) = get_json(&url) {
            if value["authorization_endpoint"].is_string() && value["token_endpoint"].is_string() {
                return Ok(value);
            }
        }
    }
    Err(format!(
        "mcp auth: {issuer} publishes no authorization server metadata"
    ))
}

/// RFC 7591: a public client at the loopback redirect, registered once and
/// remembered with the tokens it later earns.
fn register(meta: &Value, redirect: &str) -> Result<(String, Option<String>), String> {
    let Some(endpoint) = meta["registration_endpoint"].as_str() else {
        return Err(
            "mcp auth: the server has no registration endpoint; register a client and try again"
                .into(),
        );
    };
    let body = json!({
        "client_name": "axe",
        "redirect_uris": [redirect],
        "grant_types": ["authorization_code", "refresh_token"],
        "response_types": ["code"],
        "token_endpoint_auth_method": "none",
    });
    let answer = post_json(endpoint, &body, &[])?;
    Ok((
        string(&answer, "client_id")?,
        answer["client_secret"].as_str().map(str::to_string),
    ))
}

fn wait_for_code(flow: &Flow, cancel: &Cancel) -> Result<String, String> {
    let deadline = Instant::now() + Duration::from_secs(CALLBACK_SECONDS);
    loop {
        if cancel.cancelled() {
            return Err("interrupted by user (ESC); the sign-in was not completed".into());
        }
        if Instant::now() > deadline {
            return Err(format!(
                "mcp auth: no callback after {CALLBACK_SECONDS}s; run /mcp login again"
            ));
        }
        match flow.listener.accept() {
            Ok((mut socket, _)) => {
                if let Some(result) = answer(&mut socket, flow) {
                    return result;
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(e) => return Err(format!("mcp auth: callback: {e}")),
        }
    }
}

/// One connection on the loopback listener. `None` means it was not the
/// callback (a browser's favicon, a stray read) and the wait continues.
fn answer(socket: &mut TcpStream, flow: &Flow) -> Option<Result<String, String>> {
    socket.set_read_timeout(Some(Duration::from_secs(5))).ok();
    let mut reader = BufReader::new(socket.try_clone().ok()?);
    let mut line = String::new();
    reader.read_line(&mut line).ok()?;
    let mut parts = line.split_whitespace();
    let _method = parts.next()?;
    let target = parts.next()?;
    if !target.starts_with("/callback") {
        return None;
    }
    loop {
        let mut header = String::new();
        match reader.read_line(&mut header) {
            Ok(0) | Err(_) => break,
            _ if header.trim().is_empty() => break,
            _ => {}
        }
    }
    let query = target.split_once('?').map(|(_, q)| q).unwrap_or("");
    let params = parse_query(query);
    let state = params.get("state").map(String::as_str).unwrap_or("");
    let (status, body) = match params.get("error") {
        Some(error) => (
            "400 Bad Request",
            format!(
                "<h1>axe: sign-in failed</h1><p>{}</p>",
                strip_control(error)
            ),
        ),
        None if state != flow.state => (
            "400 Bad Request",
            "<h1>axe: sign-in failed</h1><p>state mismatch</p>".to_string(),
        ),
        None => match params.get("code") {
            Some(_) => (
                "200 OK",
                "<h1>axe: signed in</h1><p>You can close this tab.</p>".to_string(),
            ),
            None => (
                "400 Bad Request",
                "<h1>axe: sign-in failed</h1><p>no code</p>".to_string(),
            ),
        },
    };
    let _ = socket.write_all(
        format!(
            "HTTP/1.1 {status}\r\nContent-Type: text/html; charset=utf-8\r\n\
             Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        )
        .as_bytes(),
    );
    let _ = socket.flush();
    if status.starts_with("200") {
        Some(
            params
                .get("code")
                .cloned()
                .ok_or_else(|| "mcp auth: the callback carried no code".to_string()),
        )
    } else {
        Some(Err(params
            .get("error_description")
            .cloned()
            .or_else(|| params.get("error").cloned())
            .unwrap_or_else(|| "the server refused the sign-in".into())))
    }
}

fn get_json(url: &str) -> Result<Value, String> {
    let (ep, target) = http::split_url(url)?;
    let cancel = Cancel::new();
    let mut response = http::request_with_timeout(
        &ep,
        "GET",
        &target_for(&ep, &target),
        None,
        &[("accept".into(), "application/json".into())],
        None,
        None,
        Some(&cancel),
        Duration::from_secs(30),
    )?;
    if response.status != 200 {
        return Err(format!("{url}: HTTP {}", response.status));
    }
    let body = response.read_all().map_err(|e| e.to_string())?;
    serde_json::from_str(&body).map_err(|e| format!("{url}: {e}"))
}

/// `request` joins the endpoint's prefix with the path it is given; a target
/// from `split_url` already contains the prefix, so strip it back off.
fn target_for(ep: &http::Endpoint, target: &str) -> String {
    match target.strip_prefix(ep.prefix.as_str()) {
        Some(rest) => rest.to_string(),
        None => target.to_string(),
    }
}

fn post_json(url: &str, body: &Value, headers: &[(String, String)]) -> Result<Value, String> {
    let cancel = Cancel::new();
    let text = body.to_string();
    let (ep, target) = http::split_url(url)?;
    let mut all: Vec<(String, String)> = vec![("accept".into(), "application/json".into())];
    all.extend(headers.iter().cloned());
    let mut response = http::request_with_timeout(
        &ep,
        "POST",
        &target_for(&ep, &target),
        None,
        &all,
        Some(&text),
        None,
        Some(&cancel),
        Duration::from_secs(30),
    )?;
    let status = response.status;
    let body = response.read_all().map_err(|e| e.to_string())?;
    if !(200..300).contains(&status) {
        return Err(format!("HTTP {status}: {}", short(&body)));
    }
    serde_json::from_str(&body).map_err(|e| format!("{url}: {e}"))
}

fn token_request(
    endpoint: &str,
    form: &[(&str, String)],
    cancel: &Cancel,
) -> Result<Value, String> {
    let body = form
        .iter()
        .map(|(key, value)| format!("{}={}", encode(key), encode(value)))
        .collect::<Vec<_>>()
        .join("&");
    let (ep, target) = http::split_url(endpoint)?;
    let mut response = http::request_with_timeout(
        &ep,
        "POST",
        &target_for(&ep, &target),
        None,
        &[
            ("accept".into(), "application/json".into()),
            (
                "content-type".into(),
                "application/x-www-form-urlencoded".into(),
            ),
        ],
        Some(&body),
        None,
        Some(cancel),
        Duration::from_secs(60),
    )?;
    let status = response.status;
    let text = response.read_all().map_err(|e| e.to_string())?;
    let value: Value = serde_json::from_str(&text)
        .map_err(|_| format!("mcp auth: token endpoint: HTTP {status}: {}", short(&text)))?;
    if !(200..300).contains(&status) {
        let message = value["error_description"]
            .as_str()
            .or_else(|| value["error"].as_str())
            .unwrap_or("token request failed");
        return Err(format!("mcp auth: {message}"));
    }
    Ok(value)
}

pub fn string(value: &Value, key: &str) -> Result<String, String> {
    value[key]
        .as_str()
        .filter(|text| !text.is_empty())
        .map(str::to_string)
        .ok_or_else(|| format!("mcp auth: missing \"{key}\" in the server's answer"))
}

fn short(text: &str) -> String {
    let text = strip_control(text.trim());
    text.chars().take(400).collect()
}

fn parse_query(query: &str) -> BTreeMap<String, String> {
    let mut map = BTreeMap::new();
    for pair in query.split('&').filter(|pair| !pair.is_empty()) {
        let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
        map.insert(decode(key), decode(value));
    }
    map
}

fn random(n: usize) -> Result<Vec<u8>, String> {
    use ring::rand::SecureRandom;
    let rng = ring::rand::SystemRandom::new();
    let mut bytes = vec![0u8; n];
    rng.fill(&mut bytes)
        .map_err(|_| "mcp auth: no system randomness".to_string())?;
    Ok(bytes)
}

fn sha256(bytes: &[u8]) -> Vec<u8> {
    ring::digest::digest(&ring::digest::SHA256, bytes)
        .as_ref()
        .to_vec()
}

fn b64url(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b = [
            chunk[0],
            chunk.get(1).copied().unwrap_or(0),
            chunk.get(2).copied().unwrap_or(0),
        ];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        let take = chunk.len() + 1;
        for at in 0..take {
            let index = (n >> (18 - 6 * at)) & 0x3f;
            out.push(ALPHABET[index as usize] as char);
        }
    }
    out
}

/// Percent-encode everything outside RFC 3986's unreserved set.
fn encode(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for byte in text.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~') {
            out.push(byte as char);
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

fn decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut at = 0;
    while at < bytes.len() {
        match bytes[at] {
            b'%' if at + 2 < bytes.len() => {
                let hex = std::str::from_utf8(&bytes[at + 1..at + 3]).unwrap_or("");
                match u8::from_str_radix(hex, 16) {
                    Ok(byte) => {
                        out.push(byte);
                        at += 3;
                    }
                    Err(_) => {
                        out.push(bytes[at]);
                        at += 1;
                    }
                }
            }
            b'+' => {
                out.push(b' ');
                at += 1;
            }
            byte => {
                out.push(byte);
                at += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Open the user's browser. Failure is not an error: the URL is printed
/// either way, so a headless machine can paste it elsewhere.
pub fn open_browser(url: &str) {
    let program = if cfg!(target_os = "macos") {
        "open"
    } else {
        "xdg-open"
    };
    let _ = Command::new(program)
        .arg(url)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::testenv;
    use std::os::unix::fs::PermissionsExt;
    use std::sync::{Arc, Mutex};

    #[test]
    fn pkce_matches_the_rfc_vector() {
        // RFC 7636 appendix B.
        let verifier = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
        assert_eq!(
            b64url(&sha256(verifier.as_bytes())),
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
        );
        let verifier = b64url(&random(32).unwrap());
        assert_eq!(verifier.len(), 43);
        assert!(verifier
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_'));
    }

    #[test]
    fn query_round_trips() {
        let encoded = encode("https://x/mcp?q=a b");
        assert!(!encoded.contains(' '));
        assert!(encoded.contains("%3A"));
        let map = parse_query("code=a%20b&state=x+y&empty");
        assert_eq!(map["code"], "a b");
        assert_eq!(map["state"], "x y");
        assert_eq!(map["empty"], "");
    }

    #[test]
    fn tokens_are_stored_owner_only() {
        testenv::home("oauth-store", |_home| {
            let token = Token {
                access: "at".into(),
                refresh: Some("rt".into()),
                expires_at: Some(now() + 3600),
                client_id: "cid".into(),
                client_secret: None,
                token_endpoint: "http://as/token".into(),
            };
            assert!(!stored("s", "http://srv/mcp"));
            save("s", "http://srv/mcp", &token).unwrap();
            assert!(stored("s", "http://srv/mcp"));
            let loaded = load("s", "http://srv/mcp").unwrap();
            assert_eq!(loaded.access, "at");
            assert_eq!(loaded.refresh.as_deref(), Some("rt"));
            let mode = fs::metadata(auth_path()).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600);
            // Two names on one URL are two accounts.
            assert!(!stored("other", "http://srv/mcp"));
            assert!(remove("s", "http://srv/mcp").unwrap());
            assert!(!stored("s", "http://srv/mcp"));
        });
    }

    #[test]
    fn a_token_is_refreshed_before_it_expires() {
        // A token whose expiry is in the past gets a new access token.
        testenv::home("oauth-refresh", |_home| {
            let token = Token {
                access: "old".into(),
                refresh: Some("rt".into()),
                expires_at: Some(now().saturating_sub(10)),
                client_id: "cid".into(),
                client_secret: None,
                token_endpoint: "http://127.0.0.1:9/token".into(),
            };
            save("s", "http://srv/mcp", &token).unwrap();
            // Nothing listens on port 9: the attempt is made, and fails loudly.
            let err = access("s", "http://srv/mcp", &Cancel::new()).unwrap_err();
            assert!(err.contains("mcp auth") || err.contains("connect"), "{err}");
        });
    }

    /// A minimal authorization server: metadata, registration, authorize and
    /// token, plus the redirect the "browser" follows.
    fn mock_as() -> (String, Arc<Mutex<Vec<String>>>) {
        use std::io::{Read, Write};
        use std::net::TcpListener;
        use std::sync::Arc;

        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        listener.set_nonblocking(false).unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let seen = Arc::new(Mutex::new(Vec::new()));
        let log = seen.clone();
        let at = base.clone();
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let mut stream = stream;
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut request = String::new();
                reader.read_line(&mut request).ok();
                let mut length = 0usize;
                loop {
                    let mut line = String::new();
                    reader.read_line(&mut line).ok();
                    if line.trim().is_empty() {
                        break;
                    }
                    if let Some((name, value)) = line.split_once(':') {
                        if name.eq_ignore_ascii_case("content-length") {
                            length = value.trim().parse().unwrap_or(0);
                        }
                    }
                }
                let mut body = vec![0u8; length];
                reader.read_exact(&mut body).ok();
                let body = String::from_utf8_lossy(&body).to_string();
                let path = request.split_whitespace().nth(1).unwrap_or("/").to_string();
                log.lock().unwrap().push(format!("{path} {body}"));
                let (status, payload) = if path.starts_with("/.well-known/oauth-protected-resource")
                {
                    (
                        "200 OK",
                        json!({ "authorization_servers": [at] }).to_string(),
                    )
                } else if path.starts_with("/.well-known/oauth-authorization-server") {
                    (
                        "200 OK",
                        json!({
                            "authorization_endpoint": format!("{at}/authorize"),
                            "token_endpoint": format!("{at}/token"),
                            "registration_endpoint": format!("{at}/register"),
                        })
                        .to_string(),
                    )
                } else if path.starts_with("/register") {
                    ("201 Created", json!({ "client_id": "cid" }).to_string())
                } else if path.starts_with("/authorize") {
                    let query = path.split_once('?').map(|(_, q)| q).unwrap_or("");
                    let params = parse_query(query);
                    let redirect = params.get("redirect_uri").cloned().unwrap_or_default();
                    let state = params.get("state").cloned().unwrap_or_default();
                    ("302 Found", format!("{redirect}?code=abc&state={state}"))
                } else if path.starts_with("/token") {
                    (
                        "200 OK",
                        json!({
                            "access_token": "at",
                            "refresh_token": "rt",
                            "expires_in": 3600,
                            "token_type": "Bearer",
                        })
                        .to_string(),
                    )
                } else {
                    ("404 Not Found", "{}".to_string())
                };
                let head = if status.starts_with("302") {
                    format!(
                        "HTTP/1.1 {status}\r\nLocation: {payload}\r\nContent-Length: 0\r\n\
                         Connection: close\r\n\r\n"
                    )
                } else {
                    format!(
                        "HTTP/1.1 {status}\r\nContent-Type: application/json\r\n\
                         Content-Length: {}\r\nConnection: close\r\n\r\n{payload}",
                        payload.len()
                    )
                };
                let _ = stream.write_all(head.as_bytes());
                let _ = stream.flush();
            }
        });
        (base, seen)
    }

    #[test]
    fn a_sign_in_completes_against_a_mock_authorization_server() {
        testenv::home("oauth-signin", |_home| {
            let (base, seen) = mock_as();
            let server = format!("{base}/mcp");
            let cancel = Cancel::new();
            // The "browser": fetch the authorize URL, read the redirect, and
            // make the loopback request the redirect names.
            let browser = |url: &str| {
                let (ep, target) = http::split_url(url).unwrap();
                let response = http::request_with_timeout(
                    &ep,
                    "GET",
                    &target_for(&ep, &target),
                    None,
                    &[],
                    None,
                    None,
                    Some(&Cancel::new()),
                    Duration::from_secs(10),
                )
                .unwrap();
                assert_eq!(response.status, 302, "authorize redirects");
                let location = response.location.clone().unwrap();
                // A browser does not wait for the app to read the callback;
                // the listener only accepts once `finish` runs.
                std::thread::spawn(move || {
                    let (ep, target) = http::split_url(&location).unwrap();
                    let mut callback = http::request_with_timeout(
                        &ep,
                        "GET",
                        &target_for(&ep, &target),
                        None,
                        &[],
                        None,
                        None,
                        Some(&Cancel::new()),
                        Duration::from_secs(10),
                    )
                    .unwrap();
                    let body = callback.read_all().unwrap();
                    assert!(body.contains("signed in"), "{body}");
                });
            };
            let sign_in = begin("s", &server, &browser).expect("discovery and registration");
            assert!(sign_in.url.contains("code_challenge_method=S256"));
            let token = sign_in.finish(&cancel).expect("the code is exchanged");
            assert_eq!(token.access, "at");
            assert_eq!(token.refresh.as_deref(), Some("rt"));
            assert!(stored("s", &server));
            let requests = seen.lock().unwrap().clone();
            assert!(
                requests.iter().any(|r| r.starts_with("/register")),
                "{requests:?}"
            );
            assert!(
                requests
                    .iter()
                    .any(|r| r.starts_with("/token") && r.contains("code_verifier")),
                "{requests:?}"
            );
        });
    }
}
