//! Shared client and wire protocol helpers for graft frontends.

use std::env;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Command as StdCommand, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

#[derive(Debug, thiserror::Error)]
pub enum WireErrorKind {
    #[error("bad frame: {0}")]
    BadFrame(#[from] serde_json::Error),
}

pub type WireResult<T> = std::result::Result<T, WireErrorKind>;

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct WireRequest {
    pub id: String,
    pub op: String,
    #[serde(default)]
    pub params: Value,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct WireResponse {
    pub id: String,
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<WireError>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct WireError {
    pub code: String,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub retry: Option<Value>,
}

impl WireResponse {
    pub fn ok(id: impl Into<String>, result: Value) -> Self {
        Self {
            id: id.into(),
            ok: true,
            result: Some(result),
            error: None,
        }
    }

    pub fn error(
        id: impl Into<String>,
        code: impl Into<String>,
        message: impl Into<String>,
    ) -> Self {
        Self {
            id: id.into(),
            ok: false,
            result: None,
            error: Some(WireError {
                code: code.into(),
                message: message.into(),
                retry: None,
            }),
        }
    }

    pub fn error_with_retry(
        id: impl Into<String>,
        code: impl Into<String>,
        message: impl Into<String>,
        retry: Value,
    ) -> Self {
        Self {
            id: id.into(),
            ok: false,
            result: None,
            error: Some(WireError {
                code: code.into(),
                message: message.into(),
                retry: Some(retry),
            }),
        }
    }
}

pub fn parse_frame(line: &str) -> WireResult<WireRequest> {
    Ok(serde_json::from_str(line)?)
}

pub fn encode_response(response: &WireResponse) -> WireResult<String> {
    Ok(format!("{}\n", serde_json::to_string(response)?))
}

pub fn default_socket_path() -> PathBuf {
    if let Ok(runtime_dir) = env::var("XDG_RUNTIME_DIR") {
        return PathBuf::from(runtime_dir).join("graft").join("graft.sock");
    }
    let user = env::var("USER").unwrap_or_else(|_| "unknown".to_string());
    PathBuf::from("/tmp")
        .join(format!("graft-{user}"))
        .join("graft.sock")
}

/// Per-workspace daemon socket. Write commands prefer this over the global
/// runtime socket so a daemon started for one checkout cannot accidentally
/// service another checkout with the wrong cwd/store root.
pub fn workspace_socket_path(cwd: &Path) -> PathBuf {
    cwd.join(".graft").join("run").join("daemon.sock")
}

pub fn request(socket: &Path, op: &str, params: Value) -> Result<Value> {
    let mut stream = UnixStream::connect(socket).with_context(|| {
        format!(
            "E_NO_DAEMON: cannot connect to {}; run `graftd start --fg --socket {}`",
            socket.display(),
            socket.display()
        )
    })?;
    let frame = json!({"id":"graft-cli", "op": op, "params": params});
    writeln!(stream, "{}", serde_json::to_string(&frame)?)?;
    stream.flush()?;
    let mut line = String::new();
    BufReader::new(stream).read_line(&mut line)?;
    if line.trim().is_empty() {
        bail!("empty response from graftd");
    }
    Ok(serde_json::from_str(&line)?)
}

/// Make sure a graftd is listening on `socket` for `cwd`. If not, spawn one
/// in the background, then poll the socket until it accepts connections or
/// the deadline expires.
///
/// This mirrors the cue-shell `ensure_daemon_running` pattern: the default CLI
/// write path talks to the daemon, and the daemon transparently exists when
/// needed.
pub fn ensure_daemon(cwd: &Path, socket: &Path) -> Result<()> {
    if socket_is_live(socket) {
        return Ok(());
    }
    if socket.exists() {
        let _ = std::fs::remove_file(socket);
    }

    let mut started = false;
    let mut last_error: Option<String> = None;
    for graftd_bin in graftd_bin_candidates() {
        let mut command = StdCommand::new(&graftd_bin);
        command
            .arg("start")
            .arg("--cwd")
            .arg(cwd)
            .arg("--socket")
            .arg(socket)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        match command.output() {
            Ok(output) if output.status.success() => {
                started = true;
                break;
            }
            Ok(output) => {
                let mut combined = String::new();
                combined.push_str(&String::from_utf8_lossy(&output.stdout));
                combined.push_str(&String::from_utf8_lossy(&output.stderr));
                bail!(
                    "could not start graftd with `{graftd_bin}`: {}",
                    combined.trim()
                );
            }
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                last_error = Some(format!("`{graftd_bin}` was not found"));
            }
            Err(err) => {
                bail!("could not start graftd with `{graftd_bin}`: {err}");
            }
        }
    }
    if !started {
        bail!(
            "could not find graftd: {}; set `GRAFT_DAEMON_BIN` or put `graftd` on PATH",
            last_error.unwrap_or_else(|| "no graftd binary found".to_string())
        );
    }

    // Poll for the socket. graftd start already polls 5s internally, but
    // we also poll to cover races between the parent returning and us
    // calling connect().
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut delay = Duration::from_millis(50);
    while Instant::now() < deadline {
        if socket_is_live(socket) {
            return Ok(());
        }
        thread::sleep(delay);
        delay = (delay * 2).min(Duration::from_millis(400));
    }
    bail!(
        "graftd did not start in time at {}; check `graftd start --fg` for diagnostics",
        socket.display()
    )
}

/// `request` plus auto-spawn: connect, retry once after spawning graftd.
pub fn request_or_spawn(cwd: &Path, socket: &Path, op: &str, params: Value) -> Result<Value> {
    if !socket_is_live(socket) {
        ensure_daemon(cwd, socket)?;
    }
    request(socket, op, params)
}

fn socket_is_live(socket: &Path) -> bool {
    if !socket.exists() {
        return false;
    }
    UnixStream::connect(socket).is_ok()
}

fn graftd_bin_candidates() -> Vec<String> {
    graftd_bin_candidates_with_override(env::var("GRAFT_DAEMON_BIN").ok())
}

fn graftd_bin_candidates_with_override(daemon_bin: Option<String>) -> Vec<String> {
    if let Some(path) = daemon_bin {
        return vec![path];
    }

    let mut candidates: Vec<String> = Vec::new();
    let mut push = |candidate: String| {
        if !candidates.contains(&candidate) {
            candidates.push(candidate);
        }
    };

    if let Ok(exe) = env::current_exe() {
        push_sibling(&exe, &mut push);
    }
    if let Some(arg0) = env::args_os().next() {
        let path = PathBuf::from(arg0);
        if path.components().count() > 1 {
            let absolute = if path.is_absolute() {
                path
            } else {
                env::current_dir()
                    .ok()
                    .map(|cwd| cwd.join(&path))
                    .unwrap_or(path)
            };
            if absolute.is_file() {
                push_sibling(&absolute, &mut push);
            }
        }
    }

    push("graftd".to_string());
    candidates
}

fn push_sibling<F: FnMut(String)>(exe: &Path, push: &mut F) {
    if let Some(parent) = exe.parent() {
        let sibling = parent.join("graftd");
        if sibling.is_file() {
            push(sibling.display().to_string());
        }
        // cargo run drops binaries into target/debug/deps/<bin>-<hash>;
        // graftd lives one directory up.
        if parent.file_name().is_some_and(|name| name == "deps")
            && let Some(bin_dir) = parent.parent()
        {
            let cargo_bin = bin_dir.join("graftd");
            if cargo_bin.is_file() {
                push(cargo_bin.display().to_string());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_socket_path_is_named_graft_sock() {
        assert_eq!(
            default_socket_path()
                .file_name()
                .and_then(|value| value.to_str()),
            Some("graft.sock")
        );
    }

    #[test]
    fn parses_and_encodes_ndjson_frame() {
        let request = parse_frame(r#"{"id":"1","op":"status","params":{}}"#).unwrap();
        assert_eq!(request.id, "1");
        assert_eq!(request.op, "status");
        let encoded = encode_response(&WireResponse::ok("1", json!({"status":"ok"}))).unwrap();
        assert!(encoded.ends_with('\n'));
    }

    #[test]
    fn graftd_candidates_always_include_path_fallback() {
        // Whatever current_exe / argv0 produce, the bare
        // `graftd` PATH lookup must remain as the final fallback so a
        // graft binary launched from `cargo run` can still find a daemon
        // installed under `~/.cargo/bin`.
        let candidates = graftd_bin_candidates_with_override(None);
        assert!(
            candidates.iter().any(|candidate| candidate == "graftd"),
            "PATH fallback missing from {candidates:?}"
        );
    }

    #[test]
    fn graftd_candidates_honour_explicit_override() {
        let candidates = graftd_bin_candidates_with_override(Some("/custom/graftd".to_string()));
        assert_eq!(candidates, vec!["/custom/graftd".to_string()]);
    }

    #[test]
    fn socket_is_live_returns_false_for_missing_path() {
        let dir =
            std::env::temp_dir().join(format!("graft-client-socket-test-{}", std::process::id()));
        let socket = dir.join("missing.sock");
        assert!(!socket_is_live(&socket));
    }
}
