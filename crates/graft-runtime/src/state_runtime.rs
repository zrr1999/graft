use std::fs;
use std::path::{Component, Path, PathBuf};
use std::process::Command as ProcessCommand;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use graft_core::{Change, FileChangeKind, StateId, TreeSnapshot};
use graft_store::GraftStore;

use crate::config::{GraftConfig, load_graft_config};
use crate::repo::{materialized_snapshot_for_state, resolve_base_state};
use crate::state_label;
use crate::view::{CommandEnvelope, CommandView, RunView};

#[derive(Clone, Debug)]
pub(crate) struct ResolvedState {
    pub(crate) input: String,
    pub(crate) state: StateId,
    pub(crate) snapshot: TreeSnapshot,
}

pub(crate) fn resolve_state_ref(
    store: &GraftStore,
    config: &GraftConfig,
    reference: &str,
) -> Result<ResolvedState> {
    let state = resolve_base_state(store, config, reference)
        .with_context(|| format!("resolve state ref `{reference}`"))?;
    let snapshot = materialized_snapshot_for_state(store, config, &state)
        .with_context(|| format!("materialize snapshot for state `{}`", state_label(&state)))?;
    Ok(ResolvedState {
        input: reference.to_string(),
        state,
        snapshot,
    })
}

pub(crate) fn object_diff_summary(store: &GraftStore, from: &str, to: &str) -> Result<String> {
    let config = load_graft_config(store)?;
    let from_state = resolve_state_ref(store, &config, from)?;
    let to_state = resolve_state_ref(store, &config, to)?;
    let change = Change::from_snapshots(
        from_state.state.clone(),
        Some(&from_state.snapshot),
        to_state.state.clone(),
        &to_state.snapshot,
    );
    let summary = change.summary();
    let mut lines = vec![format!(
        "diff {} ({}) -> {} ({}): +{} ~{} -{}",
        from_state.input,
        state_label(&from_state.state),
        to_state.input,
        state_label(&to_state.state),
        summary.added,
        summary.modified,
        summary.deleted
    )];
    if summary.added == 0 && summary.modified == 0 && summary.deleted == 0 {
        lines.push("clean".to_string());
    }
    for file in change.endpoint_diff() {
        lines.push(format!("{}\t{}", file_change_symbol(file.kind), file.path));
    }
    Ok(lines.join("\n"))
}

pub(crate) fn materialize_state(
    store: &GraftStore,
    config: &GraftConfig,
    id: &str,
    dry_run: bool,
) -> Result<CommandEnvelope> {
    let resolved = resolve_state_ref(store, config, id)?;
    let destination = materialize_worktree_path(store, &resolved.state);
    if !dry_run {
        store.materialize_tree_snapshot(&resolved.snapshot, &destination)?;
    }
    Ok(CommandEnvelope {
        message: Some(if dry_run {
            format!(
                "materialization dry-run for {id}: resolved {}; would write state into {}",
                state_label(&resolved.state),
                destination.display()
            )
        } else {
            format!(
                "materialized {id}: resolved {} into {}",
                state_label(&resolved.state),
                destination.display()
            )
        }),
        registry_changed: false,
        git_changed: false,
        ..CommandEnvelope::ok()
    })
}

pub(crate) fn materialize_worktree_path(store: &GraftStore, state: &StateId) -> PathBuf {
    store
        .paths()
        .workspace_worktrees()
        .join(filesystem_safe_state_slug(state))
}

pub(crate) fn filesystem_safe_state_slug(state: &StateId) -> String {
    state_label(state)
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.') {
                ch
            } else {
                '_'
            }
        })
        .collect()
}

pub(crate) fn run_in_state(
    store: &GraftStore,
    config: &GraftConfig,
    state_ref: &str,
    cwd: Option<&Path>,
    command: &[String],
) -> Result<CommandEnvelope> {
    let command = normalized_run_command(command)?;
    let resolved = resolve_state_ref(store, config, state_ref)?;
    let state_root = run_state_temp_root(store, &resolved.state)?;
    let run_cwd_rel = normalize_run_cwd(cwd)?;
    let run_cwd = state_root.join(&run_cwd_rel);
    store.materialize_tree_snapshot(&resolved.snapshot, &state_root)?;
    if !run_cwd.is_dir() {
        let _ = fs::remove_dir_all(&state_root);
        bail!(
            "[E_RUN_CWD_NOT_FOUND] --cwd {} is not a directory inside materialized state {}",
            run_cwd_display(&run_cwd_rel),
            state_label(&resolved.state)
        );
    }
    let output = match ProcessCommand::new(&command[0])
        .args(&command[1..])
        .current_dir(&run_cwd)
        .output()
    {
        Ok(output) => output,
        Err(error) => {
            let _ = fs::remove_dir_all(&state_root);
            return Err(error)
                .with_context(|| format!("run `{}` in {}", command.join(" "), run_cwd.display()));
        }
    };
    let _ = fs::remove_dir_all(&state_root);
    Ok(CommandEnvelope {
        view: Some(CommandView::Run(RunView {
            state_ref: state_ref.to_string(),
            resolved_state: state_label(&resolved.state),
            cwd: run_cwd_display(&run_cwd_rel),
            command,
            exit_code: output.status.code().unwrap_or(-1),
            stdout: String::from_utf8_lossy(&output.stdout).to_string(),
            stderr: String::from_utf8_lossy(&output.stderr).to_string(),
        })),
        ..CommandEnvelope::ok()
    })
}

fn normalized_run_command(command: &[String]) -> Result<Vec<String>> {
    let command = if command.first().is_some_and(|arg| arg == "--") {
        &command[1..]
    } else {
        command
    };
    if command.is_empty() {
        bail!("[E_RUN_COMMAND_REQUIRED] graft run requires a command after --");
    }
    Ok(command.to_vec())
}

fn normalize_run_cwd(cwd: Option<&Path>) -> Result<PathBuf> {
    let Some(cwd) = cwd else {
        return Ok(PathBuf::new());
    };
    if cwd.as_os_str().is_empty() {
        bail!("[E_RUN_CWD_EMPTY] --cwd must not be empty");
    }
    if cwd.is_absolute() {
        bail!("[E_RUN_CWD_OUTSIDE_STATE] --cwd must be relative to the materialized state root");
    }
    let mut normalized = PathBuf::new();
    for component in cwd.components() {
        match component {
            Component::CurDir => {}
            Component::Normal(value) => normalized.push(value),
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                bail!(
                    "[E_RUN_CWD_OUTSIDE_STATE] --cwd {} escapes the materialized state root",
                    cwd.display()
                );
            }
        }
    }
    Ok(normalized)
}

fn run_cwd_display(path: &Path) -> String {
    if path.as_os_str().is_empty() {
        ".".to_string()
    } else {
        path.display().to_string()
    }
}

fn run_state_temp_root(store: &GraftStore, state: &StateId) -> Result<PathBuf> {
    let parent = store.paths().cache_tmp();
    fs::create_dir_all(&parent)?;
    let slug = filesystem_safe_state_slug(state);
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or_default();
    for attempt in 0..100 {
        let path = parent.join(format!("run-{slug}-{}-{attempt}", now));
        if !path.exists() {
            return Ok(path);
        }
    }
    bail!(
        "[E_RUN_TEMP_UNAVAILABLE] could not allocate temporary state directory under {}",
        parent.display()
    )
}

fn file_change_symbol(kind: FileChangeKind) -> &'static str {
    match kind {
        FileChangeKind::Added | FileChangeKind::Captured => "A",
        FileChangeKind::Modified => "M",
        FileChangeKind::Deleted => "D",
        FileChangeKind::Unchanged => "=",
    }
}
