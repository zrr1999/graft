use std::env;
use std::fs::{self, OpenOptions};
use std::io;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use clap::{Parser, Subcommand};
use graft_client::{
    DaemonSocketState, WireResponse, daemon_socket_path, daemon_socket_state, encode_response,
    prepare_daemon_socket_for_bind, request_result as client_request_result,
};
use graft_store::GraftStore;
use serde::Deserialize;

use crate::{DaemonState, handle_frame};

const DEFAULT_IDLE_TIMEOUT: Duration = Duration::from_secs(30 * 60);

#[derive(Parser, Debug)]
#[command(
    name = "graftd",
    about = "Graft workspace daemon (Unix socket writer)",
    long_about = "Run and control graftd, the process that owns $GRAFT_HOME/run/daemon.sock, serializes `.graft/` writes, and serves CLI wire requests."
)]
struct Cli {
    #[command(subcommand)]
    command: Option<DaemonCommand>,

    #[arg(
        long,
        global = true,
        help = "Unix socket path; defaults to $GRAFT_HOME/run/daemon.sock"
    )]
    socket: Option<PathBuf>,

    #[arg(
        long,
        global = true,
        help = "Workspace directory whose .graft/ store the daemon serves"
    )]
    cwd: Option<PathBuf>,
}

#[derive(Subcommand, Debug)]
enum DaemonCommand {
    /// Start the daemon. Without `--fg`, spawn a detached background child and
    /// wait for the socket to come up before returning.
    Start {
        #[arg(
            long,
            short = 'f',
            help = "Run in the foreground instead of spawning a detached child"
        )]
        fg: bool,
    },
    /// Run the daemon in the foreground (also used by the spawned background
    /// child). Owns $GRAFT_HOME/run/daemon.sock and daemon.pid.
    Serve,
    /// Stop the running daemon (if any) and start a fresh one.
    Restart,
    /// Check daemon status over the Unix socket.
    Status,
    /// Request graceful daemon shutdown.
    #[command(visible_aliases = ["shutdown"])]
    Stop,
}

pub fn run() {
    if let Err(error) = run_inner() {
        eprintln!("graftd: {error}");
        std::process::exit(1);
    }
}

fn run_inner() -> Result<(), Box<dyn std::error::Error>> {
    let cli = Cli::parse();
    let socket = match cli.socket {
        Some(socket) => socket,
        None => daemon_socket_path()?,
    };
    let cwd = resolve_daemon_cwd(cli.cwd)?;

    match cli.command.unwrap_or(DaemonCommand::Start { fg: false }) {
        DaemonCommand::Start { fg } => start(&cwd, &socket, fg),
        DaemonCommand::Serve => serve(&cwd, &socket),
        DaemonCommand::Restart => restart(&cwd, &socket),
        DaemonCommand::Status => request_once(&socket, "status", serde_json::json!({})),
        DaemonCommand::Stop => request_once(&socket, "shutdown", serde_json::json!({})),
    }
}

fn start(cwd: &Path, socket: &Path, foreground: bool) -> Result<(), Box<dyn std::error::Error>> {
    let _run_dir = socket_run_dir(socket)?;
    if foreground {
        return serve(cwd, socket);
    }

    // Already running? Treat as success so `graftd start` is idempotent.
    match daemon_socket_state(socket)
        .map_err(|error| -> Box<dyn std::error::Error> { error.to_string().into() })?
    {
        DaemonSocketState::Live => return Ok(()),
        DaemonSocketState::Missing => {}
        DaemonSocketState::Stale => prepare_daemon_socket_for_bind(socket)
            .map_err(|error| -> Box<dyn std::error::Error> { error.to_string().into() })?,
    }
    idle_timeout(cwd)?;

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
        match daemon_socket_state(socket)
            .map_err(|error| -> Box<dyn std::error::Error> { error.to_string().into() })?
        {
            DaemonSocketState::Live => return Ok(()),
            DaemonSocketState::Missing | DaemonSocketState::Stale => {}
        }
        thread::sleep(delay);
        delay = (delay * 2).min(Duration::from_millis(400));
    }
    Err(format!(
        "graftd child (pid {pid}) did not create socket at {} within 5s; run `graftd start --fg --cwd {} --socket {}` for diagnostics",
        socket.display(),
        cwd.display(),
        socket.display()
    )
    .into())
}

fn restart(cwd: &Path, socket: &Path) -> Result<(), Box<dyn std::error::Error>> {
    restart_after_shutdown(
        cwd,
        socket,
        Duration::from_secs(5),
        Duration::from_millis(50),
    )
}

fn restart_after_shutdown(
    cwd: &Path,
    socket: &Path,
    shutdown_timeout: Duration,
    poll_interval: Duration,
) -> Result<(), Box<dyn std::error::Error>> {
    let _run_dir = socket_run_dir(socket)?;
    if matches!(
        daemon_socket_state(socket)
            .map_err(|error| -> Box<dyn std::error::Error> { error.to_string().into() })?,
        DaemonSocketState::Live
    ) {
        let _shutdown = client_request_result(socket, "shutdown", serde_json::json!({}))?;
        // Give the previous serve loop a moment to remove the socket/PID
        // before we try to spawn a replacement.
        wait_for_socket_release(socket, shutdown_timeout, poll_interval)?;
    }
    start(cwd, socket, false)
}

fn wait_for_socket_release(
    socket: &Path,
    timeout: Duration,
    poll_interval: Duration,
) -> Result<(), Box<dyn std::error::Error>> {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        match daemon_socket_state(socket)
            .map_err(|error| -> Box<dyn std::error::Error> { error.to_string().into() })?
        {
            DaemonSocketState::Live => thread::sleep(poll_interval),
            DaemonSocketState::Missing | DaemonSocketState::Stale => return Ok(()),
        }
    }
    match daemon_socket_state(socket)
        .map_err(|error| -> Box<dyn std::error::Error> { error.to_string().into() })?
    {
        DaemonSocketState::Live => Err(format!(
            "[E_DAEMON_SHUTDOWN_TIMEOUT] graftd did not release socket {} within {}ms",
            socket.display(),
            timeout.as_millis()
        )
        .into()),
        DaemonSocketState::Missing | DaemonSocketState::Stale => Ok(()),
    }
}

fn socket_run_dir(socket: &Path) -> Result<&Path, Box<dyn std::error::Error>> {
    socket
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .ok_or_else(|| {
            format!(
                "[E_SOCKET_PARENT_REQUIRED] graftd socket path must include an explicit parent directory: {}",
                socket.display()
            )
            .into()
        })
}

fn serve(cwd: &Path, socket: &Path) -> Result<(), Box<dyn std::error::Error>> {
    let run_dir = socket_run_dir(socket)?;
    // Ownership checks must precede workspace cleanup. If another daemon is
    // live, `serve` should fail before touching pid files or run caches.
    prepare_daemon_socket_for_bind(socket)
        .map_err(|error| -> Box<dyn std::error::Error> { error.to_string().into() })?;
    fs::create_dir_all(run_dir)?;

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
        remove_dir_all_if_exists(&path)?;
        fs::create_dir_all(&path)?;
    }
    let idle_timeout = idle_timeout(cwd)?;

    // The daemon PID/socket are now anchored in $GRAFT_HOME/run (the socket
    // parent), not in each workspace. There is intentionally no `.graft/.lock`:
    // a live daemon owns writes, and stale PID/socket files are cleaned up
    // before serving.
    remove_file_if_exists(&store.paths().root().join(".lock"))?;
    let pid_path = run_dir.join("daemon.pid");
    remove_file_if_exists(&pid_path)?;
    claim_pid_file(&pid_path)?;

    // Re-check immediately before bind in case another process claimed the
    // socket while this process was preparing the workspace.
    prepare_daemon_socket_for_bind(socket)
        .map_err(|error| -> Box<dyn std::error::Error> { error.to_string().into() })?;
    let listener = UnixListener::bind(socket)
        .map_err(|err| format!("graftd cannot bind socket at {}: {err}", socket.display()))?;
    listener.set_nonblocking(true)?;

    let state = DaemonState::new(store);

    let outcome = (|| -> Result<(), Box<dyn std::error::Error>> {
        let mut last_activity = Instant::now();
        loop {
            match listener.accept() {
                Ok((stream, _addr)) => {
                    last_activity = Instant::now();
                    match handle_connection(&state, stream) {
                        Ok(true) => break,
                        Ok(false) => {}
                        Err(error) => {
                            eprintln!("graftd: connection error: {error}");
                        }
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
    let socket_cleanup = remove_file_if_exists(socket);
    let pid_cleanup = remove_file_if_exists(&pid_path);
    outcome?;
    socket_cleanup?;
    pid_cleanup?;
    Ok(())
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

fn idle_timeout(cwd: &Path) -> Result<Duration, Box<dyn std::error::Error>> {
    let path = cwd.join(".graft").join("config.toml");
    if !path.exists() {
        return Ok(DEFAULT_IDLE_TIMEOUT);
    }
    let text = fs::read_to_string(&path)
        .map_err(|err| format!("read daemon config {}: {err}", path.display()))?;
    let config: LocalDaemonConfig = toml::from_str(&text)
        .map_err(|err| format!("parse daemon config {}: {err}", path.display()))?;
    config
        .daemon
        .idle_timeout()
        .map_err(|err| format!("invalid daemon config {}: {err}", path.display()).into())
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct LocalDaemonConfig {
    #[serde(default)]
    daemon: DaemonConfig,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct DaemonConfig {
    idle_timeout_seconds: Option<u64>,
    idle_timeout_minutes: Option<u64>,
}

impl DaemonConfig {
    fn idle_timeout(&self) -> Result<Duration, &'static str> {
        match (self.idle_timeout_seconds, self.idle_timeout_minutes) {
            (Some(_), Some(_)) => {
                Err("set only one of idle_timeout_seconds or idle_timeout_minutes")
            }
            (Some(0), None) | (None, Some(0)) => Err("idle timeout must be greater than zero"),
            (Some(seconds), None) => Ok(Duration::from_secs(seconds)),
            (None, Some(minutes)) => Ok(Duration::from_secs(minutes * 60)),
            (None, None) => Ok(DEFAULT_IDLE_TIMEOUT),
        }
    }
}

fn remove_file_if_exists(path: &Path) -> Result<(), Box<dyn std::error::Error>> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(format!("remove file {}: {error}", path.display()).into()),
    }
}

fn remove_dir_all_if_exists(path: &Path) -> Result<(), Box<dyn std::error::Error>> {
    match fs::remove_dir_all(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(format!("remove directory {}: {error}", path.display()).into()),
    }
}

/// Detach the child from this process group/session so it survives the
/// parent exiting after a foreground CLI request returns to the shell.
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
    state: &DaemonState,
    mut stream: UnixStream,
) -> Result<bool, Box<dyn std::error::Error>> {
    let mut shutdown = false;
    let reader = BufReader::new(stream.try_clone()?);
    for line in reader.lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let (response, should_shutdown) = match handle_frame(state, &line) {
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
    let result = client_request_result(socket, op, params)?;
    println!("{}", serde_json::to_string(&result)?);
    Ok(())
}

fn resolve_daemon_cwd(cwd: Option<PathBuf>) -> Result<PathBuf, Box<dyn std::error::Error>> {
    resolve_daemon_cwd_with(cwd, env::current_dir)
}

fn resolve_daemon_cwd_with<F>(
    cwd: Option<PathBuf>,
    current_dir: F,
) -> Result<PathBuf, Box<dyn std::error::Error>>
where
    F: FnOnce() -> io::Result<PathBuf>,
{
    if let Some(cwd) = cwd {
        return Ok(cwd);
    }
    current_dir().map_err(|error| {
        format!("[E_CWD_UNAVAILABLE] cannot resolve default daemon cwd: {error}").into()
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    };
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn idle_timeout_defaults_when_local_config_is_missing() {
        let dir = temp_dir("missing-config");
        fs::create_dir_all(&dir).unwrap();

        assert_eq!(idle_timeout(&dir).unwrap(), DEFAULT_IDLE_TIMEOUT);

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn idle_timeout_accepts_seconds_or_minutes() {
        let seconds_dir = temp_dir("seconds-config");
        write_local_config(
            &seconds_dir,
            r#"
[daemon]
idle_timeout_seconds = 7
"#,
        );
        assert_eq!(idle_timeout(&seconds_dir).unwrap(), Duration::from_secs(7));
        let _ = fs::remove_dir_all(&seconds_dir);

        let minutes_dir = temp_dir("minutes-config");
        write_local_config(
            &minutes_dir,
            r#"
[daemon]
idle_timeout_minutes = 2
"#,
        );
        assert_eq!(
            idle_timeout(&minutes_dir).unwrap(),
            Duration::from_secs(120)
        );
        let _ = fs::remove_dir_all(&minutes_dir);
    }

    #[test]
    fn idle_timeout_rejects_ambiguous_or_invalid_config() {
        for (name, text, expected) in [
            (
                "both-config",
                r#"
[daemon]
idle_timeout_seconds = 1
idle_timeout_minutes = 1
"#,
                "set only one",
            ),
            (
                "zero-config",
                r#"
[daemon]
idle_timeout_seconds = 0
"#,
                "greater than zero",
            ),
            (
                "unknown-field-config",
                r#"
[daemon]
idle_timeout_hours = 1
"#,
                "unknown field",
            ),
            (
                "bad-type-config",
                r#"
[daemon]
idle_timeout_seconds = "fast"
"#,
                "invalid type",
            ),
        ] {
            let dir = temp_dir(name);
            write_local_config(&dir, text);

            let error = idle_timeout(&dir).unwrap_err().to_string();
            assert!(error.contains(expected), "{error}");

            let _ = fs::remove_dir_all(&dir);
        }
    }

    #[test]
    fn cli_parse_reads_global_socket_and_cwd() {
        let cli = Cli::try_parse_from([
            "graftd",
            "status",
            "--socket",
            "run/daemon.sock",
            "--cwd",
            "/workspace/project",
        ])
        .unwrap();

        assert!(matches!(cli.command, Some(DaemonCommand::Status)));
        assert_eq!(cli.socket.as_deref(), Some(Path::new("run/daemon.sock")));
        assert_eq!(cli.cwd.as_deref(), Some(Path::new("/workspace/project")));
    }

    #[test]
    fn cli_parse_defaults_to_start_without_subcommand() {
        let cli = Cli::try_parse_from(["graftd"]).unwrap();

        assert!(cli.command.is_none());
    }

    #[test]
    fn cli_parse_accepts_foreground_short_flag() {
        let cli = Cli::try_parse_from(["graftd", "start", "-f"]).unwrap();

        assert!(matches!(
            cli.command,
            Some(DaemonCommand::Start { fg: true })
        ));
    }

    #[test]
    fn socket_run_dir_requires_explicit_parent_directory() {
        let error = socket_run_dir(Path::new("graft.sock"))
            .unwrap_err()
            .to_string();

        assert!(error.contains("[E_SOCKET_PARENT_REQUIRED]"), "{error}");
        assert!(error.contains("graft.sock"), "{error}");
        assert_eq!(
            socket_run_dir(Path::new("run/graft.sock")).unwrap(),
            Path::new("run")
        );
        assert_eq!(
            socket_run_dir(Path::new("/tmp/graft.sock")).unwrap(),
            Path::new("/tmp")
        );
    }

    #[test]
    fn daemon_cwd_uses_explicit_arg_without_reading_process_cwd() {
        let cwd = resolve_daemon_cwd_with(Some(PathBuf::from("/workspace/project")), || {
            panic!("explicit --cwd must not inspect process cwd")
        })
        .unwrap();

        assert_eq!(cwd, PathBuf::from("/workspace/project"));
    }

    #[test]
    fn daemon_cwd_defaults_to_current_dir() {
        let cwd =
            resolve_daemon_cwd_with(None, || Ok(PathBuf::from("/workspace/current"))).unwrap();

        assert_eq!(cwd, PathBuf::from("/workspace/current"));
    }

    #[test]
    fn daemon_cwd_reports_unavailable_current_dir() {
        let error = resolve_daemon_cwd_with(None, || {
            Err(io::Error::new(io::ErrorKind::NotFound, "cwd vanished"))
        })
        .unwrap_err()
        .to_string();

        assert!(error.contains("[E_CWD_UNAVAILABLE]"), "{error}");
        assert!(error.contains("cwd vanished"), "{error}");
    }

    #[test]
    fn remove_file_if_exists_ignores_missing_and_rejects_directory() {
        let dir = temp_dir("remove-file-contract");
        fs::create_dir_all(&dir).unwrap();
        let missing = dir.join("missing.pid");

        remove_file_if_exists(&missing).unwrap();
        let error = remove_file_if_exists(&dir).unwrap_err().to_string();

        assert!(error.contains("remove file"), "{error}");
        assert!(dir.exists(), "directory must not be deleted as a file");

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn remove_dir_all_if_exists_ignores_missing_and_rejects_file() {
        let dir = temp_dir("remove-dir-contract");
        fs::create_dir_all(&dir).unwrap();
        let missing = dir.join("missing-cache");
        let file = dir.join("cache-path");
        fs::write(&file, "not a directory").unwrap();

        remove_dir_all_if_exists(&missing).unwrap();
        let error = remove_dir_all_if_exists(&file).unwrap_err().to_string();

        assert!(error.contains("remove directory"), "{error}");
        assert!(file.exists(), "file must not be deleted as a directory");

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn serve_rejects_live_socket_before_workspace_side_effects() {
        let workspace = temp_dir("serve-live-socket-workspace");
        let run_dir = short_temp_dir("live");
        fs::create_dir_all(&workspace).unwrap();
        fs::create_dir_all(&run_dir).unwrap();
        let socket = run_dir.join("daemon.sock");
        let pid_path = run_dir.join("daemon.pid");
        fs::write(&pid_path, "existing-pid\n").unwrap();

        let store = GraftStore::open(&workspace);
        let legacy_lock = store.paths().root().join(".lock");
        fs::create_dir_all(store.paths().cache_tmp().join("stale")).unwrap();
        fs::create_dir_all(store.paths().cache_trials().join("stale")).unwrap();
        fs::create_dir_all(store.paths().cache_worktrees().join("stale")).unwrap();
        fs::write(store.paths().cache_tmp().join("stale/file"), "tmp").unwrap();
        fs::write(store.paths().cache_trials().join("stale/file"), "trial").unwrap();
        fs::write(
            store.paths().cache_worktrees().join("stale/file"),
            "worktree",
        )
        .unwrap();
        fs::write(&legacy_lock, "legacy-lock\n").unwrap();

        let _listener = UnixListener::bind(&socket).unwrap();

        let error = serve(&workspace, &socket).unwrap_err().to_string();

        assert!(error.contains("[E_DAEMON_ALREADY_RUNNING]"), "{error}");
        assert_eq!(fs::read_to_string(&pid_path).unwrap(), "existing-pid\n");
        assert!(
            legacy_lock.exists(),
            "legacy lock marker must not be removed"
        );
        assert!(
            store.paths().cache_tmp().join("stale/file").exists(),
            "run/tmp must not be cleaned when socket is already live"
        );
        assert!(
            store.paths().cache_trials().join("stale/file").exists(),
            "run/trials must not be cleaned when socket is already live"
        );
        assert!(
            store.paths().cache_worktrees().join("stale/file").exists(),
            "run/worktrees must not be cleaned when socket is already live"
        );

        let _ = fs::remove_dir_all(&workspace);
        let _ = fs::remove_dir_all(&run_dir);
    }

    #[test]
    fn restart_fails_when_shutdown_does_not_release_socket() {
        let workspace = temp_dir("restart-timeout-workspace");
        let run_dir = short_temp_dir("restart");
        fs::create_dir_all(&workspace).unwrap();
        fs::create_dir_all(&run_dir).unwrap();
        let socket = run_dir.join("daemon.sock");
        let stubborn_daemon = StubbornDaemon::listen(&socket);

        let error = restart_after_shutdown(
            &workspace,
            &socket,
            Duration::from_millis(20),
            Duration::from_millis(1),
        )
        .unwrap_err()
        .to_string();

        assert!(error.contains("[E_DAEMON_SHUTDOWN_TIMEOUT]"), "{error}");
        assert!(
            matches!(
                daemon_socket_state(&socket).unwrap(),
                DaemonSocketState::Live
            ),
            "stubborn daemon should still own the socket"
        );

        stubborn_daemon.stop();
        let _ = fs::remove_dir_all(&workspace);
        let _ = fs::remove_dir_all(&run_dir);
    }

    struct StubbornDaemon {
        running: Arc<AtomicBool>,
        handle: thread::JoinHandle<()>,
    }

    impl StubbornDaemon {
        fn listen(socket: &Path) -> Self {
            let listener = UnixListener::bind(socket).unwrap();
            listener.set_nonblocking(true).unwrap();
            let running = Arc::new(AtomicBool::new(true));
            let thread_running = Arc::clone(&running);
            let handle = thread::spawn(move || {
                while thread_running.load(Ordering::SeqCst) {
                    match listener.accept() {
                        Ok((mut stream, _addr)) => {
                            stream.set_nonblocking(true).unwrap();
                            let mut line = String::new();
                            let mut reader = BufReader::new(stream.try_clone().unwrap());
                            for _ in 0..10 {
                                match reader.read_line(&mut line) {
                                    Ok(0) => break,
                                    Ok(_) => break,
                                    Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                                        thread::sleep(Duration::from_millis(1));
                                    }
                                    Err(error) => panic!("stubborn daemon read failed: {error}"),
                                }
                            }
                            if line.contains(r#""op":"shutdown""#) {
                                writeln!(
                                    stream,
                                    r#"{{"id":"graft-cli","ok":true,"result":{{"status":"ignored"}}}}"#
                                )
                                .unwrap();
                            }
                        }
                        Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                            thread::sleep(Duration::from_millis(1));
                        }
                        Err(error) => panic!("stubborn daemon accept failed: {error}"),
                    }
                }
            });
            Self { running, handle }
        }

        fn stop(self) {
            self.running.store(false, Ordering::SeqCst);
            self.handle.join().unwrap();
        }
    }

    fn write_local_config(dir: &Path, text: &str) {
        let graft_dir = dir.join(".graft");
        fs::create_dir_all(&graft_dir).unwrap();
        fs::write(graft_dir.join("config.toml"), text).unwrap();
    }

    fn temp_dir(name: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!(
            "graft-daemon-{name}-{}-{nanos}",
            std::process::id()
        ))
    }

    fn short_temp_dir(name: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!("gd-{name}-{}-{nanos}", std::process::id()))
    }
}
