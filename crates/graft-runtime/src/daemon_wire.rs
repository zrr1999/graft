use std::path::Path;

use anyhow::{Context, Result, bail};
use graft_client::{daemon_socket_path, request_result_or_spawn};
use graft_store::{GraftStore, WorkspaceDiscovery, default_workspace_root};

use crate::daemon_client::workspace_root_wire_string;
use crate::view::CommandEnvelope;
use crate::{Cli, Command, WorkspaceCommand, ensure_workspace_initialized};

pub(crate) fn run_via_daemon(cli: &Cli) -> Result<CommandEnvelope> {
    run_via_daemon_with_argv(cli, None)
}

pub(crate) fn run_workspace_registry_write_via_daemon(cli: &Cli) -> Result<CommandEnvelope> {
    let socket = daemon_socket_path()?;
    let (op, params) = workspace_registry_write_request(cli)?;
    let daemon_anchor = default_workspace_root();
    let result = request_result_or_spawn(&daemon_anchor, &socket, op, params)?;
    result_to_envelope(result)
}

pub(crate) fn run_via_daemon_with_argv(
    cli: &Cli,
    argv: Option<Vec<String>>,
) -> Result<CommandEnvelope> {
    let location = WorkspaceDiscovery::from_env().discover(&cli.cwd)?;
    let workspace_root = location.root().to_path_buf();
    let store = GraftStore::open(&workspace_root);
    ensure_workspace_initialized(&store)?;
    let socket = daemon_socket_path()?;
    let workspace_root_wire = workspace_root_wire_string(&workspace_root)?;
    let argv = argv.unwrap_or_else(|| daemon_argv_with_workspace_root(&workspace_root));
    let workspace_id = location
        .id()
        .ok_or_else(|| {
            anyhow::anyhow!("[E_NO_WORKSPACE_ID] cli_exec requires a resolved workspace_id")
        })?
        .to_string();
    let result = request_result_or_spawn(
        &workspace_root,
        &socket,
        "cli_exec",
        serde_json::json!({
            "argv": argv,
            "workspace_id": workspace_id,
            "workspace_root": workspace_root_wire
        }),
    )?;
    result_to_envelope(result)
}

fn workspace_registry_write_request(cli: &Cli) -> Result<(&'static str, serde_json::Value)> {
    let cwd = cwd_wire_string(&cli.cwd)?;
    match &cli.command {
        Command::Attach {
            workspace,
            status: false,
        }
        | Command::Workspace {
            command:
                WorkspaceCommand::Attach {
                    workspace,
                    status: false,
                },
        } => {
            let mut params = serde_json::json!({ "cwd": cwd });
            if let Some(workspace) = workspace {
                params
                    .as_object_mut()
                    .expect("workspace_attach params are an object")
                    .insert("workspace".to_string(), serde_json::json!(workspace));
            }
            Ok(("workspace_attach", params))
        }
        Command::Detach
        | Command::Workspace {
            command: WorkspaceCommand::Detach,
        } => Ok(("workspace_detach", serde_json::json!({ "cwd": cwd }))),
        _ => bail!("[E_INTERNAL] workspace registry write route received a non-registry command"),
    }
}

fn cwd_wire_string(cwd: &Path) -> Result<&str> {
    let cwd = cwd.to_str().ok_or_else(|| {
        anyhow::anyhow!(
            "[E_UNREPRESENTABLE_CWD] cwd contains non-UTF-8 bytes and cannot be encoded in the current daemon JSON wire protocol: {}",
            cwd.display()
        )
    })?;
    if cwd.trim().is_empty() {
        bail!("[E_BAD_PARAMS] cwd must not be empty");
    }
    Ok(cwd)
}

pub(crate) fn result_to_envelope(result: serde_json::Value) -> Result<CommandEnvelope> {
    serde_json::from_value(result)
        .context("[E_BAD_DAEMON_RESPONSE] daemon result is not a command envelope")
}

fn daemon_argv_with_workspace_root(workspace_root: &Path) -> Vec<String> {
    let mut raw = std::env::args()
        .map(|arg| {
            if arg.is_empty() {
                "graft".to_string()
            } else {
                arg
            }
        })
        .collect::<Vec<_>>();
    if raw.is_empty() {
        raw.push("graft".to_string());
    }

    let mut normalized = Vec::with_capacity(raw.len() + 2);
    normalized.push(raw[0].clone());
    let mut index = 1;
    while index < raw.len() {
        let arg = &raw[index];
        if arg == "--cwd" {
            index += 2;
            continue;
        }
        if arg.starts_with("--cwd=") {
            index += 1;
            continue;
        }
        normalized.push(arg.clone());
        index += 1;
    }
    normalized.insert(1, workspace_root.display().to_string());
    normalized.insert(1, "--cwd".to_string());
    normalized
}
