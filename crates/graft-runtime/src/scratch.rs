use std::path::Path;

use anyhow::{Context, Result, bail};
use clap::Subcommand;
use graft_client::{request_or_spawn, workspace_socket_path};
use serde_json::{Value, json};

use crate::view::CommandEnvelope;

#[derive(Subcommand, Debug)]
pub(crate) enum ScratchCommand {
    /// Check whether graftd is reachable
    Status,
    /// Open a scratch from a stored tree, candidate, or patch base
    Open {
        #[arg(long, help = "Base ref: tree:<id>, candidate:<id>, or patch:<id>")]
        base: String,
    },
    /// Read a file from a scratch
    Read {
        scratch: String,
        path: String,
        #[arg(long, default_value = "hashlines", help = "bytes, text, or hashlines")]
        mode: String,
    },
    /// Replace a file in a scratch with literal text
    Write {
        scratch: String,
        path: String,
        #[arg(long, help = "Text content to write")]
        content: String,
    },
    /// Apply raw JSON HashlineEdit array to a file in a scratch
    Edit {
        scratch: String,
        path: String,
        #[arg(long, help = "JSON array of graft_core::HashlineEdit records")]
        edits: String,
    },
    /// Diff two scratch ids
    Diff { from: String, to: String },
    /// Promote a scratch into a candidate
    Promote {
        scratch: String,
        #[arg(long = "expect", help = "Expected property on the candidate")]
        expected: Vec<String>,
        #[arg(
            long,
            default_value = "graft-cli",
            help = "Candidate provenance producer"
        )]
        producer: String,
        #[arg(long, help = "Candidate message")]
        message: Option<String>,
    },
    /// Drop an unpinned scratch
    Drop { scratch: String },
    /// Pin a scratch and return a lease
    Pin { scratch: String },
    /// Release a scratch lease
    Unpin { lease: String },
}

pub(crate) fn run_scratch_command(
    cwd: &Path,
    socket: Option<&Path>,
    command: &ScratchCommand,
) -> Result<CommandEnvelope> {
    let socket = socket
        .map(Path::to_path_buf)
        .unwrap_or_else(|| workspace_socket_path(cwd));
    let (op, params) = match command {
        ScratchCommand::Status => ("status", json!({})),
        ScratchCommand::Open { base } => ("scratch_open", json!({"base": base})),
        ScratchCommand::Read {
            scratch,
            path,
            mode,
        } => (
            "scratch_read",
            json!({"scratch": scratch, "path": path, "mode": mode}),
        ),
        ScratchCommand::Write {
            scratch,
            path,
            content,
        } => (
            "scratch_write",
            json!({"scratch": scratch, "path": path, "content": content}),
        ),
        ScratchCommand::Edit {
            scratch,
            path,
            edits,
        } => {
            let edits: Value = serde_json::from_str(edits).context("parse --edits JSON")?;
            (
                "scratch_edit",
                json!({"scratch": scratch, "path": path, "edits": edits}),
            )
        }
        ScratchCommand::Diff { from, to } => ("scratch_diff", json!({"from": from, "to": to})),
        ScratchCommand::Promote {
            scratch,
            expected,
            producer,
            message,
        } => (
            "scratch_promote",
            json!({"scratch": scratch, "expected": expected, "producer": producer, "message": message}),
        ),
        ScratchCommand::Drop { scratch } => ("scratch_drop", json!({"scratch": scratch})),
        ScratchCommand::Pin { scratch } => ("scratch_pin", json!({"scratch": scratch})),
        ScratchCommand::Unpin { lease } => ("scratch_unpin", json!({"lease": lease})),
    };
    let response = request_or_spawn(cwd, &socket, op, params)?;
    response_to_envelope(response)
}

fn response_to_envelope(response: Value) -> Result<CommandEnvelope> {
    if response.get("ok").and_then(Value::as_bool) != Some(true) {
        let error = response.get("error").cloned().unwrap_or_else(|| json!({}));
        let code = error
            .get("code")
            .and_then(Value::as_str)
            .unwrap_or("E_UNKNOWN");
        let message = error
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("unknown graftd error");
        bail!("{code}: {message}");
    }
    let result = response.get("result").cloned().unwrap_or_else(|| json!({}));
    let candidate_id = result
        .get("candidate")
        .and_then(Value::as_str)
        .map(ToString::to_string);
    Ok(CommandEnvelope {
        message: Some(render_result(&result)),
        candidate_id,
        cache_changed: result.get("candidate").is_some(),
        registry_changed: false,
        git_changed: false,
        ..CommandEnvelope::ok()
    })
}

fn render_result(result: &Value) -> String {
    if let Some(content) = result.get("content").and_then(Value::as_str) {
        return content.to_string();
    }
    serde_json::to_string_pretty(result).unwrap_or_else(|_| result.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn workspace_socket_path_uses_run_daemon_sock() {
        assert_eq!(
            workspace_socket_path(Path::new("."))
                .file_name()
                .and_then(|value| value.to_str()),
            Some("daemon.sock")
        );
    }
}
