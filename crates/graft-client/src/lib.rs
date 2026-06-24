//! Shared client and wire protocol helpers for graft frontends.

use std::env;
use std::ffi::{OsStr, OsString};
#[cfg(any(unix, test))]
use std::io;
#[cfg(unix)]
use std::io::{BufRead, BufReader, Write};
#[cfg(unix)]
use std::os::unix::fs::FileTypeExt;
#[cfg(unix)]
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Command as StdCommand, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, thiserror::Error)]
pub enum WireErrorKind {
    #[error("bad frame: {0}")]
    BadFrame(#[from] serde_json::Error),
}

pub type WireResult<T> = std::result::Result<T, WireErrorKind>;

#[cfg(any(unix, test))]
const CLIENT_REQUEST_ID: &str = "graft-cli";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DaemonSocketState {
    Missing,
    Live,
    Stale,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct WireRequest {
    pub id: String,
    pub op: String,
    pub params: Value,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct WireResponse {
    pub id: String,
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<WireError>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
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

pub fn response_result(response: Value) -> Result<Value> {
    let response: WireResponse = serde_json::from_value(response)
        .context("[E_BAD_DAEMON_RESPONSE] malformed daemon response")?;
    wire_response_result(response)
}

fn wire_response_result(response: WireResponse) -> Result<Value> {
    match (response.ok, response.result, response.error) {
        (true, Some(result), None) => Ok(result),
        (true, Some(_), Some(_)) => {
            bail!("[E_BAD_DAEMON_RESPONSE] daemon ok response included error");
        }
        (true, None, Some(_)) => {
            bail!("[E_BAD_DAEMON_RESPONSE] daemon ok response included error");
        }
        (true, None, None) => {
            bail!("[E_BAD_DAEMON_RESPONSE] daemon response missing result");
        }
        (false, Some(_), _) => {
            bail!("[E_BAD_DAEMON_RESPONSE] daemon error response included result");
        }
        (false, None, Some(error)) => {
            bail!("{}: {}", error.code, error.message);
        }
        (false, None, None) => {
            bail!("[E_BAD_DAEMON_RESPONSE] daemon error response missing error object");
        }
    }
}

/// Global daemon socket for the active `$GRAFT_HOME`.
///
/// Workspace routing is carried in the request payload (`workspace_id`) and
/// normalized argv `--cwd`, not by choosing per-workspace sockets.
pub fn daemon_socket_path() -> Result<PathBuf> {
    daemon_socket_path_from_env(env::var_os("GRAFT_HOME"), env::var_os("HOME"))
}

fn daemon_socket_path_from_env(
    graft_home: Option<OsString>,
    home: Option<OsString>,
) -> Result<PathBuf> {
    Ok(graft_home_root_from_env(graft_home, home)?
        .join("run")
        .join("daemon.sock"))
}

fn graft_home_root_from_env(
    graft_home: Option<OsString>,
    home: Option<OsString>,
) -> Result<PathBuf> {
    if let Some(graft_home) = graft_home {
        return Ok(PathBuf::from(graft_home));
    }
    if let Some(home) = home {
        return Ok(PathBuf::from(home).join(".graft"));
    }
    bail!("[E_GRAFT_HOME_UNAVAILABLE] set GRAFT_HOME or HOME to locate the global graft daemon")
}

#[cfg(unix)]
fn request_response(socket: &Path, op: &str, params: Value) -> Result<WireResponse> {
    let mut stream = UnixStream::connect(socket).with_context(|| {
        format!(
            "E_NO_DAEMON: cannot connect to {}; run `graftd start --fg --socket {}`",
            socket.display(),
            socket.display()
        )
    })?;
    let frame = request_frame(op, params);
    writeln!(stream, "{}", serde_json::to_string(&frame)?)?;
    stream.flush()?;
    let mut line = String::new();
    BufReader::new(stream).read_line(&mut line)?;
    if line.trim().is_empty() {
        bail!("empty response from graftd");
    }
    let response: WireResponse =
        serde_json::from_str(&line).context("[E_BAD_DAEMON_RESPONSE] malformed daemon response")?;
    ensure_response_id(&response)?;
    Ok(response)
}

#[cfg(not(unix))]
fn request_response(socket: &Path, _op: &str, _params: Value) -> Result<WireResponse> {
    bail!(
        "[E_DAEMON_UNSUPPORTED] graftd daemon transport uses Unix-domain sockets and is unsupported on this platform: {}",
        socket.display()
    )
}

#[cfg(any(unix, test))]
fn request_frame(op: &str, params: Value) -> WireRequest {
    WireRequest {
        id: CLIENT_REQUEST_ID.to_string(),
        op: op.to_string(),
        params,
    }
}

#[cfg(any(unix, test))]
fn ensure_response_id(response: &WireResponse) -> Result<()> {
    if response.id == CLIENT_REQUEST_ID {
        Ok(())
    } else {
        bail!(
            "[E_BAD_DAEMON_RESPONSE] daemon response id `{}` did not match request id `{}`",
            response.id,
            CLIENT_REQUEST_ID
        )
    }
}

pub fn request_result(socket: &Path, op: &str, params: Value) -> Result<Value> {
    wire_response_result(request_response(socket, op, params)?)
}

/// Make sure a graftd is listening on `socket` for `cwd`. If not, spawn one
/// in the background, then poll the socket until it accepts connections or
/// the deadline expires.
///
/// This mirrors the cue-shell `ensure_daemon_running` pattern: the default CLI
/// write path talks to the daemon, and the daemon transparently exists when
/// needed.
pub fn ensure_daemon(cwd: &Path, socket: &Path) -> Result<()> {
    match daemon_socket_state(socket)? {
        DaemonSocketState::Live => return Ok(()),
        DaemonSocketState::Missing => {}
        DaemonSocketState::Stale => remove_stale_daemon_socket(socket)?,
    }

    let mut started = false;
    let mut last_error: Option<String> = None;
    for graftd_bin in graftd_bin_candidates() {
        let graftd_bin_display = display_os_path(&graftd_bin);
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
                    "could not start graftd with `{graftd_bin_display}`: {output}",
                    output = combined.trim()
                );
            }
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                last_error = Some(format!("`{graftd_bin_display}` was not found"));
            }
            Err(err) => {
                bail!("could not start graftd with `{graftd_bin_display}`: {err}");
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
        match daemon_socket_state(socket)? {
            DaemonSocketState::Live => return Ok(()),
            DaemonSocketState::Missing | DaemonSocketState::Stale => {}
        }
        thread::sleep(delay);
        delay = (delay * 2).min(Duration::from_millis(400));
    }
    bail!(
        "graftd did not start in time at {}; check `graftd start --fg` for diagnostics",
        socket.display()
    )
}

pub fn request_result_or_spawn(
    cwd: &Path,
    socket: &Path,
    op: &str,
    params: Value,
) -> Result<Value> {
    if !matches!(daemon_socket_state(socket)?, DaemonSocketState::Live) {
        ensure_daemon(cwd, socket)?;
    }
    request_result(socket, op, params)
}

#[cfg(unix)]
pub fn daemon_socket_state(socket: &Path) -> Result<DaemonSocketState> {
    let metadata = match std::fs::symlink_metadata(socket) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Ok(DaemonSocketState::Missing);
        }
        Err(error) => {
            return Err(error)
                .with_context(|| format!("inspect graftd socket {}", socket.display()));
        }
    };
    if !metadata.file_type().is_socket() {
        bail!(
            "[E_DAEMON_SOCKET_BLOCKED] graftd socket path exists but is not a socket: {}",
            socket.display()
        );
    }
    match UnixStream::connect(socket) {
        Ok(_) => Ok(DaemonSocketState::Live),
        Err(error) => classify_daemon_socket_connect_error(socket, &error),
    }
}

#[cfg(not(unix))]
pub fn daemon_socket_state(socket: &Path) -> Result<DaemonSocketState> {
    bail!(
        "[E_DAEMON_UNSUPPORTED] graftd daemon transport uses Unix-domain sockets and is unsupported on this platform: {}",
        socket.display()
    )
}

#[cfg(any(unix, test))]
fn classify_daemon_socket_connect_error(
    socket: &Path,
    error: &io::Error,
) -> Result<DaemonSocketState> {
    match error.kind() {
        io::ErrorKind::ConnectionRefused => Ok(DaemonSocketState::Stale),
        io::ErrorKind::NotFound => Ok(DaemonSocketState::Missing),
        _ => bail!(
            "[E_DAEMON_SOCKET_UNAVAILABLE] cannot connect to graftd socket {}; refusing to remove it because it may be owned by a live daemon: {error}",
            socket.display()
        ),
    }
}

pub fn prepare_daemon_socket_for_bind(socket: &Path) -> Result<()> {
    match daemon_socket_state(socket)? {
        DaemonSocketState::Missing => Ok(()),
        DaemonSocketState::Stale => remove_stale_daemon_socket(socket),
        DaemonSocketState::Live => bail!(
            "[E_DAEMON_ALREADY_RUNNING] graftd is already listening at {}",
            socket.display()
        ),
    }
}

#[cfg(unix)]
pub fn remove_stale_daemon_socket(socket: &Path) -> Result<()> {
    let metadata = match std::fs::symlink_metadata(socket) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => {
            return Err(error)
                .with_context(|| format!("inspect stale graftd socket {}", socket.display()));
        }
    };
    if !metadata.file_type().is_socket() {
        bail!(
            "refusing to remove non-socket path while cleaning stale graftd socket {}",
            socket.display()
        );
    }
    match std::fs::remove_file(socket) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => {
            Err(error).with_context(|| format!("remove stale graftd socket {}", socket.display()))
        }
    }
}

#[cfg(not(unix))]
pub fn remove_stale_daemon_socket(socket: &Path) -> Result<()> {
    bail!(
        "[E_DAEMON_UNSUPPORTED] graftd daemon transport uses Unix-domain sockets and is unsupported on this platform: {}",
        socket.display()
    )
}

fn graftd_bin_candidates() -> Vec<OsString> {
    graftd_bin_candidates_with_override(env::var_os("GRAFT_DAEMON_BIN"))
}

fn graftd_bin_candidates_with_override(daemon_bin: Option<OsString>) -> Vec<OsString> {
    if let Some(path) = daemon_bin {
        return vec![path];
    }

    let mut candidates: Vec<OsString> = Vec::new();
    let mut push = |candidate: OsString| {
        if !candidates.iter().any(|existing| existing == &candidate) {
            candidates.push(candidate);
        }
    };

    if let Ok(exe) = env::current_exe() {
        push_sibling(&exe, &mut push);
    }
    if let Some(arg0) = env::args_os().next()
        && let Some(absolute) = arg0_executable_path(arg0, env::current_dir)
        && absolute.is_file()
    {
        push_sibling(&absolute, &mut push);
    }

    push(OsString::from("graftd"));
    candidates
}

fn arg0_executable_path<F>(arg0: OsString, current_dir: F) -> Option<PathBuf>
where
    F: FnOnce() -> std::io::Result<PathBuf>,
{
    let path = PathBuf::from(arg0);
    if path.components().count() <= 1 {
        return None;
    }
    if path.is_absolute() {
        return Some(path);
    }
    match current_dir() {
        Ok(cwd) => Some(cwd.join(path)),
        Err(_) => None,
    }
}

fn push_sibling<F: FnMut(OsString)>(exe: &Path, push: &mut F) {
    if let Some(parent) = exe.parent() {
        let sibling = parent.join("graftd");
        if sibling.is_file() {
            push(sibling.into_os_string());
        }
        // cargo run drops binaries into target/debug/deps/<bin>-<hash>;
        // graftd lives one directory up.
        if parent.file_name().is_some_and(|name| name == "deps")
            && let Some(bin_dir) = parent.parent()
        {
            let cargo_bin = bin_dir.join("graftd");
            if cargo_bin.is_file() {
                push(cargo_bin.into_os_string());
            }
        }
    }
}

fn display_os_path(path: &OsStr) -> String {
    Path::new(path).display().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn daemon_socket_path_is_global_graft_home_socket() {
        assert_eq!(
            daemon_socket_path_from_env(
                Some(OsString::from("/tmp/graft-home")),
                Some(OsString::from("/tmp/home")),
            )
            .unwrap(),
            PathBuf::from("/tmp/graft-home/run/daemon.sock")
        );
    }

    #[test]
    fn daemon_socket_path_falls_back_to_home_graft_dir() {
        assert_eq!(
            daemon_socket_path_from_env(None, Some(OsString::from("/tmp/home"))).unwrap(),
            PathBuf::from("/tmp/home/.graft/run/daemon.sock")
        );
    }

    #[test]
    fn daemon_socket_path_requires_stable_home() {
        let error = daemon_socket_path_from_env(None, None)
            .unwrap_err()
            .to_string();

        assert!(error.contains("[E_GRAFT_HOME_UNAVAILABLE]"), "{error}");
    }

    #[test]
    fn daemon_socket_path_is_named_daemon_sock() {
        assert_eq!(
            daemon_socket_path()
                .unwrap()
                .file_name()
                .and_then(|value| value.to_str()),
            Some("daemon.sock")
        );
    }

    #[cfg(unix)]
    #[test]
    fn daemon_socket_path_preserves_non_utf8_graft_home_component() {
        use std::os::unix::ffi::OsStringExt;

        let graft_home = OsString::from_vec(b"/tmp/graft-home-\xFF".to_vec());
        let path = daemon_socket_path_from_env(Some(graft_home.clone()), None).unwrap();

        assert_eq!(path, PathBuf::from(graft_home).join("run/daemon.sock"));
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
    fn request_frame_requires_params() {
        let error = parse_frame(r#"{"id":"1","op":"status"}"#)
            .unwrap_err()
            .to_string();

        assert!(error.contains("missing field `params`"), "{error}");
    }

    #[test]
    fn request_frame_rejects_unknown_envelope_fields() {
        let error = parse_frame(r#"{"id":"1","op":"status","trace_id":"x"}"#)
            .unwrap_err()
            .to_string();

        assert!(error.contains("unknown field"), "{error}");
    }

    #[test]
    fn request_frame_uses_stable_client_request_id() {
        let frame = request_frame("status", json!({}));

        assert_eq!(frame.id, CLIENT_REQUEST_ID);
        assert_eq!(frame.op, "status");
        assert_eq!(frame.params, json!({}));
    }

    #[test]
    fn response_id_must_match_request_id() {
        let ok = WireResponse::ok(CLIENT_REQUEST_ID, json!({"status": "ok"}));
        ensure_response_id(&ok).unwrap();

        let error = ensure_response_id(&WireResponse::ok("other-request", json!({})))
            .unwrap_err()
            .to_string();
        assert!(error.contains("did not match request id"), "{error}");
    }

    #[test]
    fn response_result_returns_success_payload() {
        let result = response_result(json!({
            "id": "1",
            "ok": true,
            "result": {"scratch": "scratch:abc"}
        }))
        .unwrap();

        assert_eq!(result["scratch"].as_str(), Some("scratch:abc"));
    }

    #[test]
    fn response_result_preserves_structured_daemon_errors() {
        let error = response_result(json!({
            "id": "1",
            "ok": false,
            "error": {"code": "E_BAD_PARAMS", "message": "missing field path"}
        }))
        .unwrap_err()
        .to_string();

        assert_eq!(error, "E_BAD_PARAMS: missing field path");
    }

    #[test]
    fn response_result_rejects_malformed_wire_frames() {
        for (name, frame, expected) in [
            (
                "missing-id",
                json!({"result": {"status": "ok"}}),
                "malformed daemon response",
            ),
            (
                "missing-ok",
                json!({"id": "1", "result": {"status": "ok"}}),
                "malformed daemon response",
            ),
            (
                "unknown-response-field",
                json!({"id": "1", "ok": true, "result": {}, "trace_id": "x"}),
                "malformed daemon response",
            ),
            (
                "unknown-error-field",
                json!({
                    "id": "1",
                    "ok": false,
                    "error": {"code": "E_BAD", "message": "bad", "extra": true}
                }),
                "malformed daemon response",
            ),
            (
                "ok-missing-result",
                json!({"id": "1", "ok": true}),
                "daemon response missing result",
            ),
            (
                "ok-includes-error",
                json!({
                    "id": "1",
                    "ok": true,
                    "result": {},
                    "error": {"code": "E_BAD", "message": "bad"}
                }),
                "daemon ok response included error",
            ),
            (
                "error-missing-error",
                json!({"id": "1", "ok": false}),
                "daemon error response missing error object",
            ),
            (
                "error-includes-result",
                json!({"id": "1", "ok": false, "result": {}}),
                "daemon error response included result",
            ),
        ] {
            let error = response_result(frame).unwrap_err().to_string();
            assert!(error.contains(expected), "{name}: {error}");
        }
    }

    #[test]
    fn graftd_candidates_always_include_path_fallback() {
        // Whatever current_exe / argv0 produce, the bare
        // `graftd` PATH lookup must remain as the final fallback so a
        // graft binary launched from `cargo run` can still find a daemon
        // installed under `~/.cargo/bin`.
        let candidates = graftd_bin_candidates_with_override(None);
        assert!(
            candidates
                .iter()
                .any(|candidate| candidate == OsStr::new("graftd")),
            "PATH fallback missing from {candidates:?}"
        );
    }

    #[test]
    fn graftd_candidates_honour_explicit_override() {
        let candidates =
            graftd_bin_candidates_with_override(Some(OsString::from("/custom/graftd")));
        assert_eq!(candidates, vec![OsString::from("/custom/graftd")]);
    }

    #[cfg(unix)]
    #[test]
    fn graftd_candidates_preserve_non_utf8_explicit_override() {
        use std::os::unix::ffi::OsStringExt;

        let override_path = OsString::from_vec(b"/tmp/graftd-\xFF".to_vec());
        let candidates = graftd_bin_candidates_with_override(Some(override_path.clone()));

        assert_eq!(candidates, vec![override_path]);
    }

    #[test]
    fn arg0_path_ignores_bare_program_names() {
        assert_eq!(
            arg0_executable_path(OsString::from("graft"), || {
                panic!("bare argv0 must not inspect process cwd")
            }),
            None
        );
    }

    #[test]
    fn arg0_path_preserves_absolute_path_without_reading_cwd() {
        assert_eq!(
            arg0_executable_path(OsString::from("/opt/graft/bin/graft"), || {
                panic!("absolute argv0 must not inspect process cwd")
            }),
            Some(PathBuf::from("/opt/graft/bin/graft"))
        );
    }

    #[test]
    fn arg0_path_resolves_relative_path_against_current_dir() {
        assert_eq!(
            arg0_executable_path(OsString::from("target/debug/graft"), || {
                Ok(PathBuf::from("/workspace/graft"))
            }),
            Some(PathBuf::from("/workspace/graft/target/debug/graft"))
        );
    }

    #[test]
    fn arg0_path_skips_relative_path_when_current_dir_is_unavailable() {
        let resolved = arg0_executable_path(OsString::from("target/debug/graft"), || {
            Err(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "cwd vanished",
            ))
        });

        assert_eq!(resolved, None);
    }

    #[cfg(unix)]
    #[test]
    fn socket_state_returns_missing_for_missing_path() {
        let dir = test_temp_dir("graft-client-missing-socket");
        let socket = dir.join("missing.sock");

        assert_eq!(
            daemon_socket_state(&socket).unwrap(),
            DaemonSocketState::Missing
        );
    }

    #[cfg(unix)]
    #[test]
    fn remove_stale_daemon_socket_reports_non_socket_path() {
        let dir = std::env::temp_dir().join(format!(
            "graft-client-stale-socket-dir-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let error = remove_stale_daemon_socket(&dir).unwrap_err().to_string();
        assert!(
            error.contains("refusing to remove non-socket path"),
            "{error}"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn remove_stale_daemon_socket_does_not_delete_regular_file() {
        let dir = std::env::temp_dir().join(format!(
            "graft-client-stale-socket-file-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("daemon.sock");
        std::fs::write(&path, "not a socket").unwrap();

        let error = remove_stale_daemon_socket(&path).unwrap_err().to_string();
        assert!(
            error.contains("refusing to remove non-socket path"),
            "{error}"
        );
        assert!(path.exists(), "regular file must not be deleted");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn daemon_socket_state_reports_non_socket_path() {
        let dir = test_temp_dir("graft-client-state-non-socket");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("daemon.sock");
        std::fs::write(&path, "not a socket").unwrap();

        let error = daemon_socket_state(&path).unwrap_err().to_string();

        assert!(error.contains("[E_DAEMON_SOCKET_BLOCKED]"), "{error}");
        assert!(path.exists(), "non-socket path must not be deleted");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn daemon_socket_state_reports_stale_socket_without_listener() {
        let dir = test_temp_dir("graft-client-state-stale-socket");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("daemon.sock");
        {
            let _listener = std::os::unix::net::UnixListener::bind(&path).unwrap();
        }

        assert_eq!(
            daemon_socket_state(&path).unwrap(),
            DaemonSocketState::Stale
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn daemon_socket_state_reports_live_socket() {
        let dir = test_temp_dir("graft-client-state-live-socket");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("daemon.sock");
        let _listener = std::os::unix::net::UnixListener::bind(&path).unwrap();

        assert_eq!(daemon_socket_state(&path).unwrap(), DaemonSocketState::Live);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn daemon_socket_connect_permission_errors_are_not_stale() {
        let error = io::Error::new(io::ErrorKind::PermissionDenied, "permission denied");

        let message = classify_daemon_socket_connect_error(Path::new("/tmp/graft.sock"), &error)
            .unwrap_err()
            .to_string();

        assert!(
            message.contains("[E_DAEMON_SOCKET_UNAVAILABLE]"),
            "{message}"
        );
        assert!(message.contains("refusing to remove it"), "{message}");
    }

    #[cfg(unix)]
    #[test]
    fn prepare_daemon_socket_for_bind_removes_only_stale_socket() {
        let dir = test_temp_dir("graft-client-prepare-stale-socket");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("daemon.sock");
        {
            let _listener = std::os::unix::net::UnixListener::bind(&path).unwrap();
        }

        prepare_daemon_socket_for_bind(&path).unwrap();

        assert!(!path.exists(), "stale socket should be removed before bind");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn prepare_daemon_socket_for_bind_rejects_live_socket() {
        let dir = test_temp_dir("graft-client-prepare-live-socket");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("daemon.sock");
        let _listener = std::os::unix::net::UnixListener::bind(&path).unwrap();

        let error = prepare_daemon_socket_for_bind(&path)
            .unwrap_err()
            .to_string();

        assert!(error.contains("[E_DAEMON_ALREADY_RUNNING]"), "{error}");
        assert!(path.exists(), "live socket path must not be deleted");

        let _ = std::fs::remove_dir_all(&dir);
    }

    fn test_temp_dir(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!("{name}-{}", std::process::id()))
    }
}
