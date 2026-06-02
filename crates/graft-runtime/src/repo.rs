use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use clap::Subcommand;
use graft_core::{BaseRefSpec, StateId, TreeSnapshot};
use graft_repo::GixBackend;
use graft_store::GraftStore;

use crate::config::{GraftConfig, RepoConfig, write_repo_lock_entry};
use crate::view::CommandEnvelope;

#[derive(Subcommand, Debug)]
pub(crate) enum RepoCommand {
    /// Add a repository entry to graft.toml
    Add {
        /// Stable project-local repository id
        repo_id: String,
        /// Git URL or local path to clone/fetch
        url: String,
        #[arg(long, help = "Explicit checkout/cache path for this repo")]
        path: Option<PathBuf>,
        #[arg(long, help = "Disable automatic clone-on-demand for this repo")]
        no_auto_clone: bool,
        #[arg(long, help = "Default branch label to record for humans")]
        default_branch: Option<String>,
    },
    /// List repositories configured in graft.toml
    List,
    /// Clone or fetch one configured repository, or every repository when omitted
    Sync {
        /// Configured repo id to sync
        repo: Option<String>,
    },
    /// Resolve configured repo treeishes into graft.lock
    Lock {
        /// Configured repo id to lock, or every repository when omitted
        repo: Option<String>,
    },
    /// Fetch then refresh configured repo lock entries
    Update {
        /// Configured repo id to update, or every repository when omitted
        repo: Option<String>,
    },
}

pub(crate) fn run_repo_command(
    cwd: &Path,
    config: &GraftConfig,
    command: &RepoCommand,
) -> Result<CommandEnvelope> {
    match command {
        RepoCommand::Add {
            repo_id,
            url,
            path,
            no_auto_clone,
            default_branch,
        } => {
            if config.repos.contains_key(repo_id) {
                bail!("repo {repo_id} is already configured");
            }
            validate_repo_id(repo_id)?;
            if url.trim().is_empty() {
                bail!("repo {repo_id} must set a non-empty url");
            }
            let repo = RepoConfig {
                url: url.clone(),
                path: path.clone(),
                auto_clone: !*no_auto_clone,
                default_branch: default_branch.clone(),
            };
            append_repo_config(cwd, repo_id, &repo)?;
            Ok(CommandEnvelope {
                message: Some(format!("added repo {repo_id}")),
                cache_changed: false,
                registry_changed: false,
                git_changed: false,
                ..CommandEnvelope::ok()
            })
        }
        RepoCommand::List => {
            let mut lines = Vec::new();
            for (repo_id, repo) in &config.repos {
                let path = config.repo_path(cwd, repo_id)?;
                let status = if path.exists() { "present" } else { "missing" };
                lines.push(format!(
                    "{repo_id}\t{status}\t{}\t{}",
                    path.display(),
                    repo.url
                ));
            }
            Ok(CommandEnvelope {
                message: Some(if lines.is_empty() {
                    "no repositories configured".to_string()
                } else {
                    lines.join("\n")
                }),
                ..CommandEnvelope::ok()
            })
        }
        RepoCommand::Sync { repo } => {
            let git = GixBackend;
            let repo_ids = match repo {
                Some(repo_id) => vec![repo_id.clone()],
                None => config.repos.keys().cloned().collect(),
            };
            let mut lines = Vec::new();
            for repo_id in repo_ids {
                let repo_config = config
                    .repos
                    .get(&repo_id)
                    .with_context(|| format!("unknown repo id {repo_id}"))?;
                let path = config.repo_path(cwd, &repo_id)?;
                let ensured = git.ensure_repo(&repo_config.url, &path)?;
                if !ensured.cloned {
                    git.sync_repo(&path)?;
                }
                lines.push(format!(
                    "{repo_id}\t{}\t{}",
                    if ensured.cloned { "cloned" } else { "synced" },
                    path.display()
                ));
            }
            Ok(CommandEnvelope {
                message: Some(lines.join("\n")),
                ..CommandEnvelope::ok()
            })
        }
        RepoCommand::Lock { repo } => lock_repos(cwd, config, repo.as_ref(), false),
        RepoCommand::Update { repo } => lock_repos(cwd, config, repo.as_ref(), true),
    }
}

fn lock_repos(
    cwd: &Path,
    config: &GraftConfig,
    repo: Option<&String>,
    fetch_first: bool,
) -> Result<CommandEnvelope> {
    let store = GraftStore::open(cwd);
    let git = GixBackend;
    let repo_ids = match repo {
        Some(repo_id) => vec![repo_id.clone()],
        None => config.repos.keys().cloned().collect(),
    };
    let mut lines = Vec::new();
    for repo_id in repo_ids {
        let repo_config = config
            .repos
            .get(&repo_id)
            .with_context(|| format!("unknown repo id {repo_id}"))?;
        let path = config.repo_path(cwd, &repo_id)?;
        if repo_config.auto_clone {
            let ensured = git.ensure_repo(&repo_config.url, &path)?;
            if fetch_first && !ensured.cloned {
                git.sync_repo(&path)?;
            }
        } else if !path.exists() {
            bail!("repo {repo_id} is not cloned; run `graft repo sync {repo_id}`");
        } else if fetch_first {
            git.sync_repo(&path)?;
        }
        let treeish = repo_config.default_branch.as_deref().unwrap_or("HEAD");
        let state = git.repo_tree_state(&repo_id, &path, treeish)?;
        let StateId::RepoTree(repo_state) = state else {
            unreachable!("repo_tree_state returns repo state");
        };
        write_repo_lock_entry(&store, &repo_id, treeish, &repo_state.resolved_tree_oid)?;
        lines.push(format!(
            "{repo_id}\t{}\t{}",
            if fetch_first { "updated" } else { "locked" },
            repo_state.resolved_tree_oid
        ));
    }
    Ok(CommandEnvelope {
        message: Some(lines.join("\n")),
        registry_changed: true,
        ..CommandEnvelope::ok()
    })
}

pub(crate) fn resolve_base_state(
    store: &GraftStore,
    config: &GraftConfig,
    from: &str,
) -> Result<StateId> {
    let spec = BaseRefSpec::parse(from).with_context(|| format!("parse base ref `{from}`"))?;
    match spec {
        BaseRefSpec::Empty => {
            // graft:empty is an explicit "there is no base" sentinel for
            // environments without a Git context. We materialize it as a
            // canonical empty graft-tree so the rest of the pipeline
            // (ValidPatch, snapshot diffing, evidence) treats it like any
            // other base state.
            let empty = TreeSnapshot::new(Vec::new());
            let (tree_id, _) = store
                .write_tree_snapshot(&empty)
                .context("write empty graft-tree for graft:empty base")?;
            Ok(StateId::GraftTree(tree_id))
        }
        BaseRefSpec::GraftTree(id) => Ok(StateId::GraftTree(id)),
        BaseRefSpec::Candidate(id) => {
            let candidate = store
                .read_candidate(id.as_str())
                .with_context(|| format!("read candidate {id} for base ref `{from}`"))?;
            Ok(candidate.target_state)
        }
        BaseRefSpec::Patch(id) => {
            let patch = store
                .read_patch(id.as_str())
                .with_context(|| format!("read patch {id} for base ref `{from}`"))?;
            Ok(patch.target_state)
        }
        BaseRefSpec::Repo { repo_id, treeish } => {
            let repo_config = config
                .repos
                .get(&repo_id)
                .with_context(|| format!("unknown repo id `{repo_id}` in base ref `{from}`; declare it under [repos.{repo_id}] in graft.toml"))?;
            let repo_path = config.repo_path(store.paths().workspace(), &repo_id)?;
            let git = GixBackend;
            if repo_config.auto_clone {
                git.ensure_repo(&repo_config.url, &repo_path)?;
            } else if !repo_path.exists() {
                bail!("repo {repo_id} is not cloned; run `graft repo sync {repo_id}`");
            }
            git.repo_tree_state(&repo_id, &repo_path, &treeish)
                .with_context(|| format!("resolve base tree `{from}`"))
        }
        BaseRefSpec::GitTreeish(treeish) => {
            let git = GixBackend;
            git.tree_state(store.paths().workspace(), &treeish).map_err(|_err| {
                anyhow::anyhow!(
                    "[B001] cannot resolve git base `{treeish}` against {} — not a git repository, or `{treeish}` is not a known revision.\n  fix: pass `--from graft:empty` for a workspace with no git base, or `--from repo:<id>@<treeish>` to use a repo declared under [repos.<id>] in graft.toml.",
                    store.paths().workspace().display(),
                )
            })
        }
    }
}

pub(crate) fn base_snapshot_for_state(
    store: &GraftStore,
    config: &GraftConfig,
    state: &StateId,
) -> Result<Option<TreeSnapshot>> {
    Ok(Some(materialized_snapshot_for_state(store, config, state)?))
}

pub(crate) fn materialized_snapshot_for_state(
    store: &GraftStore,
    config: &GraftConfig,
    state: &StateId,
) -> Result<TreeSnapshot> {
    match state {
        StateId::GitTree(treeish) => {
            let git = GixBackend;
            Ok(git.tree_snapshot(
                store.paths().workspace(),
                treeish,
                Some(store.paths().object_blobs()),
            )?)
        }
        StateId::RepoTree(repo) => {
            let git = GixBackend;
            let repo_path = config.repo_path(store.paths().workspace(), &repo.repo_id)?;
            Ok(git.tree_snapshot(
                repo_path,
                &repo.resolved_tree_oid,
                Some(store.paths().object_blobs()),
            )?)
        }
        StateId::GraftTree(id) => Ok(store.read_tree_snapshot(id)?),
    }
}

fn append_repo_config(cwd: &Path, repo_id: &str, repo: &RepoConfig) -> Result<()> {
    let path = cwd.join("graft.toml");
    let mut text = fs::read_to_string(&path)
        .with_context(|| format!("read {}; run graft init first", path.display()))?;
    if !text.ends_with('\n') {
        text.push('\n');
    }
    text.push('\n');
    text.push_str(&format!("[repos.{repo_id}]\n"));
    text.push_str(&format!("url = {}\n", toml_string(&repo.url)?));
    if let Some(path) = &repo.path {
        text.push_str(&format!(
            "path = {}\n",
            toml_string(&path.to_string_lossy())?
        ));
    }
    if !repo.auto_clone {
        text.push_str("auto_clone = false\n");
    }
    if let Some(default_branch) = &repo.default_branch {
        text.push_str(&format!(
            "default_branch = {}\n",
            toml_string(default_branch)?
        ));
    }
    fs::write(&path, text).with_context(|| format!("write {}", path.display()))?;
    Ok(())
}

fn toml_string(value: &str) -> Result<String> {
    serde_json::to_string(value).context("serialize TOML string")
}

fn validate_repo_id(repo_id: &str) -> Result<()> {
    if repo_id.is_empty() {
        bail!("repo id must not be empty");
    }
    if !repo_id.bytes().all(|byte| {
        byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-' || byte == b'_'
    }) {
        bail!("invalid repo id {repo_id}; use lowercase letters, digits, '-' or '_'");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validate_repo_id_accepts_lowercase_dashed_ids() {
        assert!(validate_repo_id("graft").is_ok());
        assert!(validate_repo_id("demo_repo").is_ok());
        assert!(validate_repo_id("demo-repo-2").is_ok());
    }

    #[test]
    fn validate_repo_id_rejects_uppercase_or_empty() {
        assert!(validate_repo_id("").is_err());
        assert!(validate_repo_id("Graft").is_err());
        assert!(validate_repo_id("graft.repo").is_err());
    }
}
