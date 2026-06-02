use std::env;
use std::fs::{self, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use graft_client::{WireResponse, default_socket_path, encode_response, request};
use graft_scratch::ScratchEngine;
use graft_store::GraftStore;

use crate::handle_frame;

pub fn run() {
    if let Err(error) = run_inner() {
        eprintln!("graftd: {error}");
        std::process::exit(1);
    }
}

fn run_inner() -> Result<(), Box<dyn std::error::Error>> {
    let args = env::args().skip(1).collect::<Vec<_>>();
    let command = args.first().map(String::as_str).unwrap_or("start");
    let socket = option_value(&args, "--socket")
        .map(PathBuf::from)
        .unwrap_or_else(default_socket_path);
    let cwd = option_value(&args, "--cwd")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));

    match command {
        "start" => start(
            &cwd,
            &socket,
            has_flag(&args, "--fg") || has_flag(&args, "-f"),
        ),
        "serve" => serve(&cwd, &socket),
        "restart" => restart(&cwd, &socket),
        "status" => request_once(&socket, "status", serde_json::json!({})),
        "stop" | "shutdown" => request_once(&socket, "shutdown", serde_json::json!({})),
        "help" | "--help" | "-h" => {
            print_help();
            Ok(())
        }
        "--version" | "-V" => {
            println!("graftd {}", env!("CARGO_PKG_VERSION"));
            Ok(())
        }
        other => Err(format!(
            "unknown command {other}; expected start/serve/restart/status/stop/shutdown"
        )
        .into()),
    }
}

fn start(cwd: &Path, socket: &Path, foreground: bool) -> Result<(), Box<dyn std::error::Error>> {
    if foreground {
        return serve(cwd, socket);
    }

    // Already running? Treat as success so `graftd start` is idempotent.
    if socket_is_live(socket) {
        return Ok(());
    }
    if socket.exists() {
        // Stale socket from a previous crash; remove so the spawned child
        // can bind cleanly. Failing to remove here is non-fatal because the
        // child will attempt removal again before bind.
        let _ = fs::remove_file(socket);
    }

    // Spawn a detached child running our own `serve` so the client can
    // exit immediately. We re-exec the same binary (current_exe) rather
    // than relying on PATH so a freshly built debug binary spawns its
    // matching daemon, not whatever `graftd` lives on PATH.
    let exe = env::current_exe()?;
    let mut command = Command::new(&exe);
    command
        .arg("serve")
        .arg("--cwd")
        .arg(cwd)
        .arg("--socket")
        .arg(socket)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    detach(&mut command);
    let child = command
        .spawn()
        .map_err(|err| format!("failed to spawn graftd serve at {}: {err}", exe.display()))?;
    // We intentionally do not wait; the child has been detached. Keep
    // the PID purely informational so the parent `start` can give up
    // with a useful diagnostic if the socket never appears.
    let pid = child.id();

    // Poll for the socket; the child writes it when serve() reaches the
    // accept loop.
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut delay = Duration::from_millis(50);
    while Instant::now() < deadline {
        if socket_is_live(socket) {
            return Ok(());
        }
        thread::sleep(delay);
        delay = (delay * 2).min(Duration::from_millis(400));
    }
    Err(format!(
        "graftd child (pid {pid}) did not create socket at {} within 5s",
        socket.display()
    )
    .into())
}

fn restart(cwd: &Path, socket: &Path) -> Result<(), Box<dyn std::error::Error>> {
    if socket_is_live(socket) {
        let _ = request(socket, "shutdown", serde_json::json!({}));
        // Give the previous serve loop a moment to remove the socket/PID
        // before we try to spawn a replacement.
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            if !socket_is_live(socket) {
                break;
            }
            thread::sleep(Duration::from_millis(50));
        }
    }
    start(cwd, socket, false)
}

fn serve(cwd: &Path, socket: &Path) -> Result<(), Box<dyn std::error::Error>> {
    if let Some(parent) = socket.parent() {
        fs::create_dir_all(parent)?;
    }

    let primary_socket = cwd.join(".graft").join("run").join("daemon.sock");
    if socket != primary_socket && socket_is_live(&primary_socket) {
        return Err(format!(
            "another graftd already owns this workspace (socket {})",
            primary_socket.display()
        )
        .into());
    }

    let store = GraftStore::open(cwd);
    store.init_storage().map_err(|err| {
        format!(
            "graftd cannot initialize .graft storage at {}: {err}",
            store.paths().root().display()
        )
    })?;

    for path in [
        store.paths().cache_tmp(),
        store.paths().cache_trials(),
        store.paths().cache_worktrees(),
    ] {
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path)?;
    }

    // v2 uses the daemon PID/socket as the writer ownership anchor. There is
    // intentionally no `.graft/.lock`: a live daemon owns writes, and a stale
    // pid/socket from a crashed daemon is cleaned up before serving.
    let _ = fs::remove_file(store.paths().root().join(".lock"));
    let pid_path = store.paths().root().join("run").join("daemon.pid");
    if pid_path.exists() {
        if socket_is_live(&primary_socket) {
            return Err(format!(
                "another graftd already owns this workspace (pid file {})",
                pid_path.display()
            )
            .into());
        }
        fs::remove_file(&pid_path)?;
    }
    claim_pid_file(&pid_path)?;

    if socket.exists() {
        fs::remove_file(socket)?;
    }
    let listener = UnixListener::bind(socket)
        .map_err(|err| format!("graftd cannot bind socket at {}: {err}", socket.display()))?;
    listener.set_nonblocking(true)?;

    let engine = ScratchEngine::new(store);
    let idle_timeout = idle_timeout(cwd);

    let outcome = (|| -> Result<(), Box<dyn std::error::Error>> {
        let mut last_activity = Instant::now();
        loop {
            match listener.accept() {
                Ok((stream, _addr)) => {
                    last_activity = Instant::now();
                    if handle_connection(&engine, stream)? {
                        break;
                    }
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    if last_activity.elapsed() >= idle_timeout {
                        break;
                    }
                    thread::sleep(Duration::from_millis(50));
                }
                Err(error) => return Err(error.into()),
            }
        }
        Ok(())
    })();

    // Always tear down side state, regardless of whether the loop returned
    // an error: leaving a stale socket or pid file would confuse the next
    // `graftd start`.
    let _ = fs::remove_file(socket);
    let _ = fs::remove_file(&pid_path);
    outcome
}

fn claim_pid_file(pid_path: &Path) -> Result<(), Box<dyn std::error::Error>> {
    if let Some(parent) = pid_path.parent() {
        fs::create_dir_all(parent)?;
    }
    match OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(pid_path)
    {
        Ok(mut file) => {
            writeln!(file, "{}", std::process::id())?;
            Ok(())
        }
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => Err(format!(
            "another graftd already owns this workspace (pid file {})",
            pid_path.display()
        )
        .into()),
        Err(error) => Err(error.into()),
    }
}

fn idle_timeout(cwd: &Path) -> Duration {
    let path = cwd.join(".graft").join("config.toml");
    let text = fs::read_to_string(path).unwrap_or_default();
    let mut in_daemon = false;
    for raw in text.lines() {
        let line = raw.split("#").next().unwrap_or("").trim();
        if line.starts_with("[") && line.ends_with("]") {
            in_daemon = line == "[daemon]";
            continue;
        }
        if !in_daemon {
            continue;
        }
        if let Some(value) = parse_u64_setting(line, "idle_timeout_seconds") {
            return Duration::from_secs(value.max(1));
        }
        if let Some(value) = parse_u64_setting(line, "idle_timeout_minutes") {
            return Duration::from_secs(value.max(1) * 60);
        }
    }
    Duration::from_secs(30 * 60)
}

fn parse_u64_setting(line: &str, key: &str) -> Option<u64> {
    let (left, right) = line.split_once("=")?;
    if left.trim() != key {
        return None;
    }
    right.trim().trim_matches('"').parse().ok()
}

/// Quick liveness check for a daemon socket: file exists *and* connect
/// succeeds. A bare exists() check is not enough because socket files
/// linger after a graftd crash.
fn socket_is_live(socket: &Path) -> bool {
    if !socket.exists() {
        return false;
    }
    UnixStream::connect(socket).is_ok()
}

/// Detach the child from this process group/session so it survives the
/// parent exiting (e.g. when `graft create` returns to the shell).
#[cfg(unix)]
fn detach(command: &mut Command) {
    use std::os::unix::process::CommandExt;
    // SAFETY: setsid in a freshly forked child is a documented use of
    // pre_exec; it has no thread-safety implications because Command
    // forks before invoking the closure.
    unsafe {
        command.pre_exec(|| {
            // Become the leader of a new session so a parent terminal
            // closing does not SIGHUP the daemon.
            if libc_setsid() < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
}

#[cfg(unix)]
#[cfg(unix)]
unsafe extern "C" {
    fn setsid() -> i32;
}

#[cfg(unix)]
#[inline]
fn libc_setsid() -> i32 {
    unsafe { setsid() }
}

fn handle_connection(
    engine: &ScratchEngine,
    mut stream: UnixStream,
) -> Result<bool, Box<dyn std::error::Error>> {
    let mut shutdown = false;
    let reader = BufReader::new(stream.try_clone()?);
    for line in reader.lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let (response, should_shutdown) = match handle_frame(engine, &line) {
            Ok(value) => value,
            Err(error) => (
                WireResponse::error("unknown", "E_BAD_FRAME", error.to_string()),
                false,
            ),
        };
        stream.write_all(encode_response(&response)?.as_bytes())?;
        stream.flush()?;
        shutdown |= should_shutdown;
        if shutdown {
            break;
        }
    }
    Ok(shutdown)
}

fn request_once(
    socket: &Path,
    op: &str,
    params: serde_json::Value,
) -> Result<(), Box<dyn std::error::Error>> {
    let response = request(socket, op, params)?;
    println!("{}", serde_json::to_string(&response)?);
    Ok(())
}

fn option_value(args: &[String], name: &str) -> Option<String> {
    args.windows(2)
        .find(|window| window[0] == name)
        .map(|window| window[1].clone())
}

fn has_flag(args: &[String], name: &str) -> bool {
    args.iter().any(|arg| arg == name)
}

fn print_help() {
    println!(
        "graftd {}\n\nUsage:\n  graftd start [--fg] [--cwd PATH] [--socket PATH]\n  graftd serve [--cwd PATH] [--socket PATH]\n  graftd restart [--cwd PATH] [--socket PATH]\n  graftd status [--socket PATH]\n  graftd stop [--socket PATH]\n  graftd shutdown [--socket PATH]\n\nCommands:\n  start     Start the daemon. Without --fg, spawn a detached background\n            child and wait for the socket to come up before returning.\n  serve     Run the daemon in the foreground (also used by the spawned\n            background child). Holds .graft/.lock for its whole lifetime.\n  restart   Stop the running daemon (if any) and start a fresh one.\n  status    Check daemon status over the Unix socket.\n  stop      Request graceful daemon shutdown.\n  shutdown  Alias for stop.",
        env!("CARGO_PKG_VERSION")
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn option_value_reads_socket_path() {
        assert_eq!(
            option_value(&["--socket".into(), "sock".into()], "--socket"),
            Some("sock".to_string())
        );
    }

    #[test]
    fn has_flag_finds_foreground_aliases() {
        assert!(has_flag(&["start".into(), "--fg".into()], "--fg"));
        assert!(!has_flag(&["start".into()], "--fg"));
    }
}
