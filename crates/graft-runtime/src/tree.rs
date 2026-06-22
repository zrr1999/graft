use std::path::Path;

use anyhow::{Context, Result, bail};
use clap::{Args, Subcommand};
use graft_client::{daemon_socket_path, request_result_or_spawn};
use graft_core::BaseRefSpec;
use graft_store::{
    GraftStore, TreeGrepOptions, TreeGrepResult, TreeListOptions, TreeListResult, TreeMetadata,
};
use serde_json::{Value, json};

use crate::config::GraftConfig;
use crate::daemon_client::add_workspace_route;
use crate::presentation::state_label;
use crate::repo::{materialized_snapshot_for_state, resolve_base_state};
use crate::view::CommandEnvelope;

#[derive(Args, Debug)]
pub(crate) struct TreeSource {
    #[arg(
        long,
        value_name = "BASE",
        required_unless_present = "from",
        conflicts_with = "from",
        help = "Base ref to inspect: graft:empty, tree:<id>, candidate:<id>, patch:<id>, repo:<id>@<treeish>, or a workspace Git treeish"
    )]
    base: Option<String>,
    #[arg(
        long,
        value_name = "SCRATCH",
        required_unless_present = "base",
        conflicts_with = "base",
        help = "Live scratch id to inspect through graftd"
    )]
    from: Option<String>,
    #[arg(
        long,
        value_name = "REPO",
        requires = "base",
        help = "Repo id that scopes a bare --base treeish; omitted means workspace"
    )]
    repo: Option<String>,
}

#[derive(Subcommand, Debug)]
pub(crate) enum TreeCommand {
    /// List file paths in a base tree or live scratch
    List {
        #[command(flatten)]
        source: TreeSource,
        #[arg(long, help = "Only include paths under this virtual directory")]
        path: Option<String>,
        #[arg(
            long,
            help = "Only include paths matching this simple '*' wildcard glob"
        )]
        glob: Option<String>,
        #[arg(long, help = "Maximum entries to return")]
        limit: Option<usize>,
    },
    /// Search UTF-8 text blobs in a base tree or live scratch
    Grep {
        #[command(flatten)]
        source: TreeSource,
        #[arg(help = "Literal text pattern to search for")]
        pattern: String,
        #[arg(long, help = "Only search paths under this virtual directory")]
        path: Option<String>,
        #[arg(
            long,
            help = "Only search paths matching this simple '*' wildcard glob"
        )]
        glob: Option<String>,
        #[arg(long, help = "Maximum matches to return")]
        limit: Option<usize>,
    },
    /// Read metadata for a file or directory without dumping file bytes
    #[command(alias = "read-metadata")]
    Metadata {
        #[command(flatten)]
        source: TreeSource,
        #[arg(help = "Virtual file or directory path to inspect; use '.' for the root")]
        path: String,
    },
}

pub(crate) fn run_tree_command(
    store: &GraftStore,
    config: &GraftConfig,
    workspace_root: &Path,
    workspace_id: Option<&str>,
    socket: Option<&Path>,
    command: &TreeCommand,
) -> Result<CommandEnvelope> {
    let context = TreeRunContext {
        store,
        config,
        workspace_root,
        workspace_id,
        socket,
    };
    match command {
        TreeCommand::List {
            source,
            path,
            glob,
            limit,
        } => context.tree_result(
            source,
            "tree_list",
            json!({"path": path, "glob": glob, "limit": limit}),
            |store, source| {
                let snapshot = source.snapshot;
                let result = store.tree_list(
                    &snapshot,
                    &TreeListOptions {
                        path: path.clone(),
                        glob: glob.clone(),
                        limit: *limit,
                    },
                )?;
                Ok(json_with_source(
                    source.result_source,
                    "list",
                    serde_json::to_value(result)?,
                ))
            },
            render_list_message,
        ),
        TreeCommand::Grep {
            source,
            pattern,
            path,
            glob,
            limit,
        } => context.tree_result(
            source,
            "tree_grep",
            json!({"pattern": pattern, "path": path, "glob": glob, "limit": limit}),
            |store, source| {
                let snapshot = source.snapshot;
                let result = store.tree_grep(
                    &snapshot,
                    &TreeGrepOptions {
                        pattern: pattern.clone(),
                        path: path.clone(),
                        glob: glob.clone(),
                        limit: *limit,
                    },
                )?;
                Ok(json_with_source(
                    source.result_source,
                    "grep",
                    serde_json::to_value(result)?,
                ))
            },
            render_grep_message,
        ),
        TreeCommand::Metadata { source, path } => context.tree_result(
            source,
            "tree_metadata",
            json!({"path": path}),
            |store, source| {
                let snapshot = source.snapshot;
                let result = store.tree_metadata(&snapshot, path)?;
                Ok(json_with_source(
                    source.result_source,
                    "metadata",
                    serde_json::to_value(result)?,
                ))
            },
            render_metadata_message,
        ),
    }
}

struct ResolvedTreeSource {
    snapshot: graft_core::TreeSnapshot,
    result_source: Value,
}

struct TreeRunContext<'a> {
    store: &'a GraftStore,
    config: &'a GraftConfig,
    workspace_root: &'a Path,
    workspace_id: Option<&'a str>,
    socket: Option<&'a Path>,
}

impl TreeRunContext<'_> {
    fn tree_result(
        &self,
        source: &TreeSource,
        daemon_op: &str,
        mut params: Value,
        base_handler: impl FnOnce(&GraftStore, ResolvedTreeSource) -> Result<Value>,
        render_message: fn(&Value) -> String,
    ) -> Result<CommandEnvelope> {
        let result = if let Some(scratch) = source.from.as_deref() {
            let workspace_id = self.workspace_id.ok_or_else(|| {
                anyhow::anyhow!("[E_NO_WORKSPACE_ID] tree --from requires a resolved workspace_id")
            })?;
            let Some(object) = params.as_object_mut() else {
                bail!("[E_BAD_PARAMS] tree daemon params must be a JSON object");
            };
            object.retain(|_, value| !value.is_null());
            object.insert("scratch".to_string(), json!(scratch));
            add_workspace_route(&mut params, self.workspace_root, workspace_id)?;
            let socket = match self.socket {
                Some(socket) => socket.to_path_buf(),
                None => daemon_socket_path()?,
            };
            request_result_or_spawn(self.workspace_root, &socket, daemon_op, params)?
        } else {
            let resolved = resolve_base_tree_source(self.store, self.config, source)?;
            base_handler(self.store, resolved)?
        };
        let message = render_message(&result);
        Ok(CommandEnvelope {
            message: Some(message),
            result: Some(result),
            ..CommandEnvelope::ok()
        })
    }
}

fn resolve_base_tree_source(
    store: &GraftStore,
    config: &GraftConfig,
    source: &TreeSource,
) -> Result<ResolvedTreeSource> {
    let base = source
        .base
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("[E_BAD_PARAMS] tree source requires --base or --from"))?;
    let base = scoped_base_ref(base, source.repo.as_deref())?;
    let state = resolve_base_state(store, config, &base)
        .with_context(|| format!("resolve tree base `{base}`"))?;
    let snapshot = materialized_snapshot_for_state(store, config, &state)
        .with_context(|| format!("snapshot tree base `{base}`"))?;
    let resolved_state = state_label(&state);
    Ok(ResolvedTreeSource {
        snapshot,
        result_source: json!({
            "kind": "base",
            "base": base,
            "resolved_state": resolved_state
        }),
    })
}

fn scoped_base_ref(base: &str, repo: Option<&str>) -> Result<String> {
    let Some(repo_id) = repo else {
        return Ok(base.to_string());
    };
    match BaseRefSpec::parse(base).with_context(|| format!("parse tree base `{base}`"))? {
        BaseRefSpec::GitTreeish(treeish) => Ok(format!("repo:{repo_id}@{treeish}")),
        other => bail!(
            "[E_BAD_PARAMS] --repo only scopes a bare --base treeish; got `{}`",
            other.display()
        ),
    }
}

fn json_with_source(source: Value, operation: &str, mut payload: Value) -> Value {
    let Some(object) = payload.as_object_mut() else {
        return json!({"source": source, "operation": operation, "data": payload});
    };
    object.insert("source".to_string(), source);
    object.insert("operation".to_string(), json!(operation));
    payload
}

fn render_list_message(result: &Value) -> String {
    let entries = result
        .get("entries")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|entry| entry.get("path").and_then(Value::as_str))
        .collect::<Vec<_>>();
    if entries.is_empty() {
        "no tree entries matched".to_string()
    } else {
        entries.join("\n")
    }
}

fn render_grep_message(result: &Value) -> String {
    let matches = result
        .get("matches")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|entry| {
            let path = entry.get("path")?.as_str()?;
            let line = entry.get("line")?.as_u64()?;
            let text = entry.get("text")?.as_str()?;
            Some(format!("{path}:{line}:{text}"))
        })
        .collect::<Vec<_>>();
    if matches.is_empty() {
        "no text matches found".to_string()
    } else {
        matches.join("\n")
    }
}

fn render_metadata_message(result: &Value) -> String {
    let path = result.get("path").and_then(Value::as_str).unwrap_or("");
    let kind = result
        .get("kind")
        .and_then(Value::as_str)
        .unwrap_or("unknown");
    let size = result.get("size").and_then(Value::as_u64);
    let is_utf8_text = result.get("is_utf8_text").and_then(Value::as_bool);
    match (size, is_utf8_text) {
        (Some(size), Some(is_utf8_text)) => {
            format!("{path}\t{kind}\tsize={size}\tutf8_text={is_utf8_text}")
        }
        _ => format!("{path}\t{kind}"),
    }
}

#[allow(dead_code)]
fn _assert_result_types_are_serializable(
    _list: TreeListResult,
    _grep: TreeGrepResult,
    _metadata: TreeMetadata,
) {
}
