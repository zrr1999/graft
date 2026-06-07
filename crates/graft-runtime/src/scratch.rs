use std::path::Path;

use anyhow::{Context, Result, bail};
use clap::{Args, Subcommand};
use graft_client::{daemon_socket_path, request_result_or_spawn};
use graft_core::{BaseRefSpec, Change, ScratchNode, StateId, scratch_id};
use graft_store::GraftStore;
use serde_json::{Map, Value, json};

use crate::config::load_graft_config_metadata;
use crate::daemon_client::{
    add_workspace_route, render_json_result, require_bool_field, require_string_array_field,
    require_u64_field, required_string_field,
};
use crate::repo::{materialized_snapshot_for_state, resolve_base_state};
use crate::view::CommandEnvelope;

#[derive(Args, Debug)]
pub(crate) struct ScratchSource {
    #[arg(
        long,
        value_name = "BASE",
        required_unless_present = "from",
        conflicts_with = "from",
        help = "Base ref for the first scratch operation; bare refs resolve in --repo or workspace context"
    )]
    base: Option<String>,
    #[arg(
        long,
        value_name = "SCRATCH",
        required_unless_present = "base",
        conflicts_with = "base",
        help = "Scratch id to continue editing"
    )]
    from: Option<String>,
    #[arg(
        long,
        value_name = "REPO",
        requires = "base",
        help = "Repo id that scopes --base treeish resolution; omitted means workspace"
    )]
    repo: Option<String>,
}

#[derive(Subcommand, Debug)]
pub(crate) enum ScratchCommand {
    /// Check whether graftd is reachable
    Status,
    /// Read a file from a base tree or scratch
    Read {
        #[command(flatten)]
        source: ScratchSource,
        path: String,
        #[arg(long, default_value = "hashlines", help = "bytes, text, or hashlines")]
        mode: String,
    },
    /// Replace a file in a new or existing scratch with literal text
    Write {
        #[command(flatten)]
        source: ScratchSource,
        path: String,
        #[arg(long, help = "Text content to write")]
        content: String,
    },
    /// Apply raw JSON HashlineEdit array to a file in a new or existing scratch
    Edit {
        #[command(flatten)]
        source: ScratchSource,
        path: String,
        #[arg(long, help = "JSON array of graft_core::HashlineEdit records")]
        edits: String,
    },
    /// Delete a file from a new or existing scratch
    #[command(alias = "rm")]
    Delete {
        #[command(flatten)]
        source: ScratchSource,
        path: String,
    },
    /// Capture the current workspace into a scratch, then restore cwd to the base
    Capture {
        #[arg(
            long,
            value_name = "BASE",
            help = "Base ref to restore cwd to after capture"
        )]
        base: String,
        #[arg(
            long,
            value_name = "REPO",
            help = "Repo id that scopes --base treeish resolution; omitted means workspace"
        )]
        repo: Option<String>,
        #[arg(
            long,
            help = "Report the capture and restore plan without writing scratch state or cwd"
        )]
        dry_run: bool,
    },
    /// Diff two scratch ids
    Diff { from: String, to: String },
    /// Drop an unpinned scratch
    Drop { scratch: String },
    /// Pin a scratch and return a lease
    Pin { scratch: String },
    /// Release a scratch lease
    Unpin { lease: String },
}

pub(crate) fn run_scratch_command(
    workspace_root: &Path,
    workspace_id: &str,
    socket: Option<&Path>,
    command: &ScratchCommand,
) -> Result<CommandEnvelope> {
    let socket = match socket {
        Some(socket) => socket.to_path_buf(),
        None => daemon_socket_path()?,
    };
    let (op, mut params, contract) = match command {
        ScratchCommand::Status => {
            let result = request_result_or_spawn(workspace_root, &socket, "status", json!({}))?;
            return result_to_envelope(result, ScratchResultContract::Status);
        }
        ScratchCommand::Read { source, path, mode } => (
            "scratch_read",
            params_with_source(
                workspace_root,
                source,
                [("path", json!(path)), ("mode", json!(mode))],
            )?,
            ScratchResultContract::Read,
        ),
        ScratchCommand::Write {
            source,
            path,
            content,
        } => (
            "scratch_write",
            params_with_source(
                workspace_root,
                source,
                [("path", json!(path)), ("content", json!(content))],
            )?,
            ScratchResultContract::Write,
        ),
        ScratchCommand::Edit {
            source,
            path,
            edits,
        } => {
            let edits: Value = serde_json::from_str(edits).context("parse --edits JSON")?;
            (
                "scratch_edit",
                params_with_source(
                    workspace_root,
                    source,
                    [("path", json!(path)), ("edits", edits)],
                )?,
                ScratchResultContract::Edit,
            )
        }
        ScratchCommand::Delete { source, path } => (
            "scratch_delete",
            params_with_source(workspace_root, source, [("path", json!(path))])?,
            ScratchResultContract::Delete,
        ),
        ScratchCommand::Capture {
            base,
            repo,
            dry_run,
        } => {
            return run_scratch_capture(
                workspace_root,
                workspace_id,
                &socket,
                base,
                repo.as_deref(),
                *dry_run,
            );
        }
        ScratchCommand::Diff { from, to } => (
            "scratch_diff",
            json!({"from": from, "to": to}),
            ScratchResultContract::Diff,
        ),
        ScratchCommand::Drop { scratch } => (
            "scratch_drop",
            json!({"scratch": scratch}),
            ScratchResultContract::Drop,
        ),
        ScratchCommand::Pin { scratch } => (
            "scratch_pin",
            json!({"scratch": scratch}),
            ScratchResultContract::Pin,
        ),
        ScratchCommand::Unpin { lease } => (
            "scratch_unpin",
            json!({"lease": lease}),
            ScratchResultContract::Unpin,
        ),
    };
    add_workspace_route(&mut params, workspace_root, workspace_id)?;
    let result = request_result_or_spawn(workspace_root, &socket, op, params)?;
    result_to_envelope(result, contract)
}

pub(crate) fn run_scratch_status(cwd: &Path, socket: Option<&Path>) -> Result<CommandEnvelope> {
    let socket = match socket {
        Some(socket) => socket.to_path_buf(),
        None => daemon_socket_path()?,
    };
    let result = request_result_or_spawn(cwd, &socket, "status", json!({}))?;
    result_to_envelope(result, ScratchResultContract::Status)
}

#[derive(Clone, Copy, Debug)]
enum ScratchResultContract {
    Status,
    Read,
    Write,
    Edit,
    Delete,
    Capture,
    Diff,
    Drop,
    Pin,
    Unpin,
}

fn params_with_source(
    workspace_root: &Path,
    source: &ScratchSource,
    extra: impl IntoIterator<Item = (&'static str, Value)>,
) -> Result<Value> {
    let mut params = Map::new();
    if let Some(base) = &source.base {
        match scratch_base_params(workspace_root, source.repo.as_deref(), base)? {
            ScratchBaseParams::Raw(base) => {
                params.insert("base".to_string(), json!(base));
            }
            ScratchBaseParams::Materialized {
                base_state,
                tree_id,
            } => {
                params.insert("base_state".to_string(), serde_json::to_value(base_state)?);
                params.insert("base_tree".to_string(), json!(tree_id));
            }
        }
    }
    if let Some(from) = &source.from {
        if source.repo.is_some() {
            bail!("[E_BAD_PARAMS] --repo only scopes --base; omit it when using --from");
        }
        params.insert("from".to_string(), json!(from));
    }
    for (key, value) in extra {
        params.insert(key.to_string(), value);
    }
    Ok(Value::Object(params))
}

enum ScratchBaseParams {
    Raw(String),
    Materialized {
        base_state: graft_core::StateId,
        tree_id: String,
    },
}

fn scratch_base_params(
    workspace_root: &Path,
    repo: Option<&str>,
    base: &str,
) -> Result<ScratchBaseParams> {
    let base = match repo {
        Some(repo_id) => repo_context_base(repo_id, base)?,
        None => base.to_string(),
    };
    match BaseRefSpec::parse(&base).with_context(|| format!("parse scratch base `{base}`"))? {
        BaseRefSpec::Empty
        | BaseRefSpec::GraftTree(_)
        | BaseRefSpec::Candidate(_)
        | BaseRefSpec::Patch(_) => Ok(ScratchBaseParams::Raw(base)),
        BaseRefSpec::GitTreeish(_) | BaseRefSpec::Repo { .. } => {
            materialized_scratch_base(workspace_root, &base)
        }
    }
}

fn repo_context_base(repo_id: &str, base: &str) -> Result<String> {
    let repo_id = repo_id.trim();
    if repo_id.is_empty() {
        bail!("[E_BAD_PARAMS] --repo must not be empty");
    }
    if repo_id == "workspace" {
        bail!(
            "[E_BAD_PARAMS] `workspace` is a reserved scope name; omit --repo for workspace bases"
        );
    }
    match BaseRefSpec::parse(base).with_context(|| format!("parse scratch base `{base}`"))? {
        BaseRefSpec::GitTreeish(treeish) => Ok(format!("repo:{repo_id}@{treeish}")),
        _ => bail!(
            "[E_BAD_PARAMS] --repo only selects the repo context for a bare --base treeish; got `{base}`"
        ),
    }
}

fn materialized_scratch_base(workspace_root: &Path, base: &str) -> Result<ScratchBaseParams> {
    let store = GraftStore::open(workspace_root);
    let config = load_graft_config_metadata(&store)?;
    let base_state = resolve_base_state(&store, &config, base)?;
    let snapshot = materialized_snapshot_for_state(&store, &config, &base_state)?;
    let (tree_id, _) = store.write_tree_snapshot(&snapshot)?;
    Ok(ScratchBaseParams::Materialized {
        base_state,
        tree_id,
    })
}

fn resolved_capture_base(
    workspace_root: &Path,
    repo: Option<&str>,
    base: &str,
) -> Result<(StateId, String, graft_core::TreeSnapshot)> {
    let base = match repo {
        Some(repo_id) => repo_context_base(repo_id, base)?,
        None => base.to_string(),
    };
    let store = GraftStore::open(workspace_root);
    let config = load_graft_config_metadata(&store)?;
    let base_state = resolve_base_state(&store, &config, &base)?;
    let base_snapshot = materialized_snapshot_for_state(&store, &config, &base_state)?;
    let (base_tree_id, _) = store.write_tree_snapshot(&base_snapshot)?;
    Ok((base_state, base_tree_id, base_snapshot))
}

fn run_scratch_capture(
    workspace_root: &Path,
    workspace_id: &str,
    socket: &Path,
    base: &str,
    repo: Option<&str>,
    dry_run: bool,
) -> Result<CommandEnvelope> {
    let store = GraftStore::open(workspace_root);
    store.init_storage()?;
    let (base_state, base_tree_id, base_snapshot) =
        resolved_capture_base(workspace_root, repo, base)?;
    let captured_snapshot = store.capture_worktree_snapshot(workspace_root)?;
    let target_snapshot = store.capture_target_snapshot(&base_snapshot, &captured_snapshot);
    let target_tree_id = target_snapshot.id()?;
    let change = Change::from_snapshots(
        base_state.clone(),
        Some(&base_snapshot),
        StateId::GraftTree(target_tree_id.clone()),
        &target_snapshot,
    );
    let changed_paths = change.changed_paths();
    if changed_paths.is_empty() {
        bail!("[E_EMPTY_CAPTURE] scratch capture found no changes; cwd left unchanged");
    }

    if dry_run {
        let scratch = scratch_id(&ScratchNode::root(base_state, target_tree_id.clone()))?;
        let result = json!({
            "scratch": scratch,
            "base_tree": base_tree_id,
            "target_tree": target_tree_id,
            "changed_paths": changed_paths,
            "would_restore_paths": changed_paths,
            "dry_run": true
        });
        return result_to_envelope(result, ScratchResultContract::Capture);
    }

    store.write_tree_snapshot(&target_snapshot)?;
    let mut params = json!({
        "base_state": base_state,
        "base_tree": base_tree_id,
        "target_tree": target_tree_id
    });
    add_workspace_route(&mut params, workspace_root, workspace_id)?;
    let result = request_result_or_spawn(workspace_root, socket, "scratch_capture", params)?;
    let mut envelope = result_to_envelope(result, ScratchResultContract::Capture)?;
    store.restore_worktree_paths(&base_snapshot, workspace_root, &changed_paths)?;
    envelope.message = Some(format!(
        "{}\nrestored cwd paths: {}",
        envelope.message.unwrap_or_default(),
        changed_paths.join(", ")
    ));
    Ok(envelope)
}

fn result_to_envelope(result: Value, contract: ScratchResultContract) -> Result<CommandEnvelope> {
    validate_result_contract(&result, contract)?;
    let candidate_id = result
        .get("candidate")
        .and_then(Value::as_str)
        .map(ToString::to_string);
    Ok(CommandEnvelope {
        message: Some(render_result(&result)?),
        candidate_id,
        cache_changed: result.get("candidate").is_some(),
        registry_changed: false,
        git_changed: false,
        ..CommandEnvelope::ok()
    })
}

fn validate_result_contract(result: &Value, contract: ScratchResultContract) -> Result<()> {
    match contract {
        ScratchResultContract::Status => {
            require_status_fields(result)?;
        }
        ScratchResultContract::Read => {
            require_scratch_path_fields(result, "scratch_read")?;
            required_string_field(result, "scratch_read", "file_view_hash")?;
            required_string_field(result, "scratch_read", "content")?;
            require_u64_field(result, "scratch_read", "bytes_len")?;
        }
        ScratchResultContract::Write => {
            require_scratch_path_fields(result, "scratch_write")?;
            require_string_array_field(result, "scratch_write", "changed_paths")?;
            required_string_field(result, "scratch_write", "content_hash")?;
            require_u64_field(result, "scratch_write", "size")?;
        }
        ScratchResultContract::Edit => {
            require_scratch_path_fields(result, "scratch_edit")?;
            require_string_array_field(result, "scratch_edit", "changed_paths")?;
            required_string_field(result, "scratch_edit", "updated_anchors")?;
        }
        ScratchResultContract::Delete => {
            require_scratch_path_fields(result, "scratch_delete")?;
            require_string_array_field(result, "scratch_delete", "changed_paths")?;
        }
        ScratchResultContract::Capture => {
            required_string_field(result, "scratch_capture", "scratch")?;
            required_string_field(result, "scratch_capture", "base_tree")?;
            required_string_field(result, "scratch_capture", "target_tree")?;
            require_string_array_field(result, "scratch_capture", "changed_paths")?;
        }
        ScratchResultContract::Diff => {
            required_string_field(result, "scratch_diff", "from")?;
            required_string_field(result, "scratch_diff", "to")?;
            require_string_array_field(result, "scratch_diff", "changed_paths")?;
        }
        ScratchResultContract::Drop => {
            required_string_field(result, "scratch_drop", "scratch")?;
            require_bool_field(result, "scratch_drop", "dropped")?;
        }
        ScratchResultContract::Pin | ScratchResultContract::Unpin => {
            let context = match contract {
                ScratchResultContract::Pin => "scratch_pin",
                ScratchResultContract::Unpin => "scratch_unpin",
                _ => unreachable!(),
            };
            required_string_field(result, context, "scratch")?;
            required_string_field(result, context, "lease")?;
            require_u64_field(result, context, "pinned")?;
        }
    }
    Ok(())
}

fn require_status_fields(result: &Value) -> Result<()> {
    required_string_field(result, "status", "status")?;
    required_string_field(result, "status", "daemon")?;
    Ok(())
}

fn require_scratch_path_fields(result: &Value, context: &str) -> Result<()> {
    required_string_field(result, context, "scratch")?;
    required_string_field(result, context, "path")?;
    Ok(())
}

fn render_result(result: &Value) -> Result<String> {
    if let Some(content) = result.get("content").and_then(Value::as_str) {
        return Ok(content.to_string());
    }
    render_json_result(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn daemon_socket_path_uses_run_daemon_sock() {
        assert_eq!(
            daemon_socket_path()
                .unwrap()
                .file_name()
                .and_then(|value| value.to_str()),
            Some("daemon.sock")
        );
    }

    #[test]
    fn params_with_source_translates_cli_source_flags_to_wire_fields() {
        let base_source = ScratchSource {
            base: Some("graft:empty".to_string()),
            from: None,
            repo: None,
        };
        let base_params =
            params_with_source(Path::new("."), &base_source, [("path", json!("note.txt"))])
                .unwrap();
        assert_eq!(base_params["base"].as_str(), Some("graft:empty"));
        assert_eq!(base_params["path"].as_str(), Some("note.txt"));
        assert!(base_params.get("from").is_none());

        let from_source = ScratchSource {
            base: None,
            from: Some("scratch:abc".to_string()),
            repo: None,
        };
        let from_params =
            params_with_source(Path::new("."), &from_source, [("content", json!("hello"))])
                .unwrap();
        assert_eq!(from_params["from"].as_str(), Some("scratch:abc"));
        assert_eq!(from_params["content"].as_str(), Some("hello"));
        assert!(from_params.get("base").is_none());
    }

    #[test]
    fn repo_context_base_rewrites_bare_treeish_only() {
        assert_eq!(repo_context_base("C", "main").unwrap(), "repo:C@main");
        assert!(repo_context_base("workspace", "main").is_err());
        assert!(repo_context_base("C", "graft:empty").is_err());
        assert!(repo_context_base("C", "patch:abc").is_err());
    }

    #[test]
    fn result_to_envelope_accepts_valid_write_payload() {
        let envelope = result_to_envelope(
            json!({
                "parent": "scratch:parent",
                "scratch": "scratch:next",
                "path": "note.txt",
                "changed_paths": ["note.txt"],
                "content_hash": "blob:abc",
                "size": 4
            }),
            ScratchResultContract::Write,
        )
        .unwrap();

        assert!(envelope.message.unwrap().contains("scratch:next"));
        assert!(envelope.candidate_id.is_none());
    }

    #[test]
    fn result_to_envelope_accepts_valid_drop_payload() {
        let envelope = result_to_envelope(
            json!({
                "scratch": "scratch:next",
                "dropped": true
            }),
            ScratchResultContract::Drop,
        )
        .unwrap();

        assert!(envelope.message.unwrap().contains("\"dropped\": true"));
    }

    #[test]
    fn result_to_envelope_accepts_valid_edit_payload() {
        let envelope = result_to_envelope(
            json!({
                "parent": "scratch:parent",
                "scratch": "scratch:next",
                "path": "note.txt",
                "changed_paths": ["note.txt"],
                "updated_anchors": "1#abc:hello"
            }),
            ScratchResultContract::Edit,
        )
        .unwrap();

        assert!(envelope.message.unwrap().contains("updated_anchors"));
    }

    #[test]
    fn result_to_envelope_accepts_valid_capture_payload() {
        let envelope = result_to_envelope(
            json!({
                "scratch": "scratch:next",
                "base_tree": "tree:base",
                "target_tree": "tree:target",
                "changed_paths": ["note.txt"]
            }),
            ScratchResultContract::Capture,
        )
        .unwrap();

        assert!(envelope.message.unwrap().contains("scratch:next"));
        assert!(envelope.candidate_id.is_none());
    }

    #[test]
    fn result_to_envelope_accepts_valid_pin_payload() {
        let envelope = result_to_envelope(
            json!({
                "scratch": "scratch:next",
                "lease": "lease:1",
                "pinned": 1
            }),
            ScratchResultContract::Pin,
        )
        .unwrap();

        assert!(envelope.message.unwrap().contains("\"pinned\": 1"));
    }

    #[test]
    fn result_to_envelope_requires_scratch_success_contract() {
        for (contract, result, expected) in [
            (
                ScratchResultContract::Status,
                json!({"status": "ok"}),
                "missing string field `daemon`",
            ),
            (
                ScratchResultContract::Read,
                json!({
                    "scratch": "scratch:next",
                    "path": "note.txt",
                    "content": "hello",
                    "bytes_len": 5
                }),
                "missing string field `file_view_hash`",
            ),
            (
                ScratchResultContract::Write,
                json!({
                    "scratch": "scratch:next",
                    "path": "note.txt",
                    "changed_paths": ["note.txt"],
                    "content_hash": "blob:abc"
                }),
                "missing integer field `size`",
            ),
            (
                ScratchResultContract::Edit,
                json!({
                    "scratch": "scratch:next",
                    "path": "note.txt",
                    "changed_paths": ["note.txt", false],
                    "updated_anchors": "fresh"
                }),
                "field `changed_paths` item 1 must be string",
            ),
            (
                ScratchResultContract::Capture,
                json!({
                    "scratch": "scratch:next",
                    "base_tree": "tree:base",
                    "target_tree": "tree:target"
                }),
                "missing string array field `changed_paths`",
            ),
            (
                ScratchResultContract::Pin,
                json!({
                    "scratch": "scratch:next",
                    "pinned": true
                }),
                "missing string field `lease`",
            ),
        ] {
            let error = result_to_envelope(result, contract)
                .unwrap_err()
                .to_string();
            assert!(error.contains(expected), "{error}");
        }
    }
}
