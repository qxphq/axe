//! The piped contract, from outside the process.
//!
//! Without a terminal, stdout is the model's text and what a line explicitly
//! asked for; the harness's own account of a turn — a tool call, a notice —
//! goes to stderr, so `axe < prompts 2>/dev/null` is the answers alone. Both
//! sinks write with `print!`/`eprintln!`, so only a child process can see the
//! split: this test runs the binary against a loopback mock.
//!
//! It also pins the two early answers: `--version` and `--help` need no config
//! and no network.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::thread;
use std::time::{Duration, Instant};

/// A scratch directory named for the test, emptied first: the binary under test
/// is the child, so the process id is not enough.
fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("axe-piped-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("scratch directory");
    dir
}

/// An SSE body carrying one chunk and the stream's own terminator.
fn sse(delta: &str, finish: &str) -> String {
    let chunk = format!(
        "data: {{\"choices\":[{{\"index\":0,\"delta\":{delta},\"finish_reason\":{finish}}}]}}\n\n"
    );
    format!("{chunk}data: [DONE]\n\n")
}

/// One request off the wire: its head and its body.
fn read_request(stream: &mut TcpStream) -> (String, String) {
    let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
    let mut raw = Vec::new();
    let mut chunk = [0u8; 4096];
    loop {
        let read = stream.read(&mut chunk).unwrap_or(0);
        if read == 0 {
            break;
        }
        raw.extend_from_slice(&chunk[..read]);
        let text = String::from_utf8_lossy(&raw).into_owned();
        if let Some((head, body)) = text.split_once("\r\n\r\n") {
            let want = head
                .lines()
                .find_map(|line| {
                    let (name, value) = line.split_once(':')?;
                    name.eq_ignore_ascii_case("content-length")
                        .then(|| value.trim().parse::<usize>().ok())
                        .flatten()
                })
                .unwrap_or(0);
            if body.len() >= want {
                return (head.to_string(), body.to_string());
            }
        }
    }
    (String::new(), String::new())
}

fn answer(stream: &mut TcpStream, body: &str, content_type: &str) {
    let reply = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let _ = stream.write_all(reply.as_bytes());
    let _ = stream.flush();
}

/// A provider on loopback: a model list, then one tool call for a request whose
/// last message is the user's, and the answer for a request that carries the
/// tool's result.
fn provider() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback");
    let port = listener.local_addr().expect("loopback addr").port();
    thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { return };
            let (head, body) = read_request(&mut stream);
            if head.starts_with("GET /v1/models") {
                answer(
                    &mut stream,
                    "{\"data\":[{\"id\":\"m\",\"context_length\":200000}]}",
                    "application/json",
                );
            } else if body.contains("\"role\":\"tool\"") {
                answer(
                    &mut stream,
                    &sse("{\"content\":\"the answer\"}", "null"),
                    "text/event-stream",
                );
            } else {
                let call = sse(
                    "{\"tool_calls\":[{\"index\":0,\"id\":\"c1\",\"type\":\"function\",\"function\":{\"name\":\"read\",\"arguments\":\"{\\\"path\\\":\\\"notes.txt\\\"}\"}}]}",
                    "\"tool_calls\"",
                );
                answer(&mut stream, &call, "text/event-stream");
            }
        }
    });
    port
}

/// Run the binary with a piped stdin and a bounded wait: a test that hangs is a
/// test that fails, not a job that never finishes.
fn run(dir: &Path, args: &[&str], input: Option<&str>, envs: &[(&str, PathBuf)]) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_axe"));
    command
        .args(args)
        .current_dir(dir)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for (name, value) in envs {
        command.env(name, value);
    }
    let mut child = command.spawn().expect("spawn axe");
    if let Some(input) = input {
        child
            .stdin
            .as_mut()
            .expect("stdin")
            .write_all(input.as_bytes())
            .expect("write stdin");
    }
    drop(child.stdin.take());
    let start = Instant::now();
    while child.try_wait().expect("wait").is_none() {
        if start.elapsed() > Duration::from_secs(30) {
            let _ = child.kill();
            panic!("axe did not finish in 30s");
        }
        thread::sleep(Duration::from_millis(20));
    }
    child.wait_with_output().expect("output")
}

#[test]
fn without_a_terminal_the_answer_is_stdout_and_the_tool_call_is_stderr() {
    let dir = scratch("split");
    let port = provider();
    let config = dir.join("config/axe");
    std::fs::create_dir_all(&config).expect("config dir");
    std::fs::write(
        config.join("config.json"),
        format!("{{\"providers\":{{\"m\":{{\"base_url\":\"http://127.0.0.1:{port}/v1\"}}}},\"model\":\"m/m\"}}"),
    )
    .expect("config");
    std::fs::write(dir.join("notes.txt"), "hello\n").expect("notes");

    let out = run(
        &dir,
        &[],
        Some("read notes.txt\n"),
        &[
            ("HOME", dir.clone()),
            ("XDG_CONFIG_HOME", dir.join("config")),
            ("XDG_STATE_HOME", dir.join("state")),
        ],
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "stderr: {stderr}");
    // The answer is the stdout text, and nothing else is.
    assert!(stdout.contains("the answer"), "{stdout:?}");
    assert!(!stdout.contains("read("), "a call on stdout: {stdout:?}");
    // The call is the harness's account of the turn: stderr.
    assert!(stderr.contains("read(notes.txt)"), "{stderr:?}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn version_and_help_answer_before_the_config_or_the_network() {
    // A home with no config at all: both must answer without one.
    let dir = scratch("version");
    for (flag, want) in [
        ("--version", format!("axe {}", env!("CARGO_PKG_VERSION"))),
        ("--help", "usage: axe".to_string()),
    ] {
        let out = run(
            &dir,
            &[flag],
            None,
            &[
                ("HOME", dir.clone()),
                ("XDG_CONFIG_HOME", dir.join("config")),
                ("XDG_STATE_HOME", dir.join("state")),
            ],
        );
        let stdout = String::from_utf8_lossy(&out.stdout);
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(out.status.success(), "{flag}: {stderr}");
        assert!(stdout.contains(&want), "{flag}: {stdout:?}");
        assert!(!stderr.contains("config"), "{flag}: {stderr:?}");
    }
    let _ = std::fs::remove_dir_all(&dir);
}
