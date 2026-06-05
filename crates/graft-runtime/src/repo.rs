use std::fs;
use std::path::Path;

use anyhow::{Context, Result, bail};
use clap::Subcommand;
use graft_core::{BaseRefSpec, StateId, TreeSnapshot};
use graft_repo::GixBackend;
use graft_store::GraftStore;

use crate::config::{
    GraftConfig, RepoConfig, require_repo_lock_current, validate_repo_default_branch,
    write_repo_lock_entry,
};
use crate::view::CommandEnvelope;

#[derive(Subcommand, Debug)]
pub(crate) enum RepoCommand {
    /// Add a repository entry to graft.toml
    Add {
        /// Stable project-local repository id
        repo_id: String,
        /// Git URL or local path to clone/fetch
        url: String,
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
    workspace_root: &Path,
    config: &GraftConfig,
    command: &RepoCommand,
) -> Result<CommandEnvelope> {
    match command {
        RepoCommand::Add {
            repo_id,
            url,
            default_branch,
        } => {
            if config.repos.contains_key(repo_id) {
                bail!("repo {repo_id} is already configured");
            }
            validate_repo_id(repo_id)?;
            if url.trim().is_empty() {
                bail!("repo {repo_id} must set a non-empty url");
            }
            validate_repo_default_branch(repo_id, default_branch.as_deref())?;
            let git = GixBackend;
            let path = config.repos_root_path(workspace_root)?.join(repo_id);
            let ensured = git.ensure_repo(url, &path)?;
            if !ensured.cloned {
                git.sync_repo(&path)?;
            }
            let default_branch = match default_branch {
                Some(default_branch) => Some(default_branch.clone()),
                None => Some(git.remote_default_branch(&path)?.with_context(|| {
                    format!(
                        "repo {repo_id} remote default branch is unavailable; pass --default-branch"
                    )
                })?),
            };
            let repo = RepoConfig {
                url: url.clone(),
                default_branch,
            };
            append_repo_config(workspace_root, repo_id, &repo)?;

            let mut updated_config = config.clone();
            updated_config.repos.insert(repo_id.clone(), repo);
            let mut envelope = lock_repos(workspace_root, &updated_config, Some(repo_id), false)?;
            envelope.message = Some(match envelope.message {
                Some(message) if !message.is_empty() => format!("added repo {repo_id}\n{message}"),
                _ => format!("added repo {repo_id}"),
            });
            Ok(envelope)
        }
        RepoCommand::List => {
            let mut lines = Vec::new();
            for (repo_id, repo) in &config.repos {
                let path = config.repo_path(workspace_root, repo_id)?;
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
                let path = config.repo_path(workspace_root, &repo_id)?;
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
        RepoCommand::Lock { repo } => lock_repos(workspace_root, config, repo.as_ref(), false),
        RepoCommand::Update { repo } => lock_repos(workspace_root, config, repo.as_ref(), true),
    }
}

fn lock_repos(
    workspace_root: &Path,
    config: &GraftConfig,
    repo: Option<&String>,
    fetch_first: bool,
) -> Result<CommandEnvelope> {
    let store = GraftStore::open(workspace_root);
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
        let path = config.repo_path(workspace_root, &repo_id)?;
        let ensured = git.ensure_repo(&repo_config.url, &path)?;
        if fetch_first && !ensured.cloned {
            git.sync_repo(&path)?;
        }
        let treeish = repo_config.default_branch.as_deref().unwrap_or("HEAD");
        let state = git.repo_tree_state(&repo_id, &path, treeish)?;
        let StateId::RepoTree(repo_state) = state else {
            unreachable!("repo_tree_state returns repo state");
        };
        write_repo_lock_entry(
            &store,
            &repo_id,
            &repo_config.url,
            treeish,
            &repo_state.resolved_tree_oid,
        )?;
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
            // (patch integrity, snapshot diffing, evidence) treats it like
            // any other base state.
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
            let locked = require_repo_lock_current(store, &repo_id, repo_config, &treeish)?;
            let repo_path = config.repo_path(store.paths().workspace(), &repo_id)?;
            let git = GixBackend;
            git.ensure_repo(&repo_config.url, &repo_path)?;
            Ok(StateId::RepoTree(graft_core::RepoBaseState::new(
                repo_id,
                locked.treeish,
                locked.resolved_oid,
            )))
        }
        BaseRefSpec::GitTreeish(treeish) => {
            let git = GixBackend;
            git.tree_state(store.paths().workspace(), &treeish).map_err(|err| {
                anyhow::anyhow!(
                    "[B001] cannot resolve git base `{treeish}` against {} — not a git repository, or `{treeish}` is not a known revision.\n  source: {err}\n  fix: start scratch edits with `--base graft:empty` for a workspace with no git base, or use `repo:<id>@<treeish>` where a base ref is accepted after declaring [repos.<id>] in graft.toml.",
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
            let repo_config = config
                .repos
                .get(&repo.repo_id)
                .with_context(|| format!("unknown repo id {}", repo.repo_id))?;
            let repo_path = config.repo_path(store.paths().workspace(), &repo.repo_id)?;
            git.ensure_repo(&repo_config.url, &repo_path)?;
            Ok(git.tree_snapshot(
                repo_path,
                &repo.resolved_tree_oid,
                Some(store.paths().object_blobs()),
            )?)
        }
        StateId::GraftTree(id) => Ok(store.read_tree_snapshot(id)?),
    }
}

fn append_repo_config(workspace_root: &Path, repo_id: &str, repo: &RepoConfig) -> Result<()> {
    let path = workspace_root.join("graft.toml");
    let mut text = fs::read_to_string(&path)
        .with_context(|| format!("read {}; run graft init first", path.display()))?;
    if !text.ends_with('\n') {
        text.push('\n');
    }
    text.push('\n');
    text.push_str(&format!("[repos.{repo_id}]\n"));
    text.push_str(&format!("url = {}\n", toml_string(&repo.url)?));
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
    if repo_id == "workspace" {
        bail!("repo id `workspace` is reserved for the workspace scope name");
    }
    if !repo_id
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
    {
        bail!("invalid repo id {repo_id}; use ASCII letters, digits, '-' or '_'");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use std::path::{Path, PathBuf};
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn validate_repo_id_accepts_ascii_scope_ids() {
        assert!(validate_repo_id("graft").is_ok());
        assert!(validate_repo_id("C").is_ok());
        assert!(validate_repo_id("demo_repo").is_ok());
        assert!(validate_repo_id("demo-repo-2").is_ok());
    }

    #[test]
    fn validate_repo_id_rejects_reserved_empty_or_invalid() {
        assert!(validate_repo_id("").is_err());
        assert!(validate_repo_id("workspace").is_err());
        assert!(validate_repo_id("graft.repo").is_err());
    }

    #[test]
    fn repo_add_rejects_empty_default_branch_before_writing_config() {
        let workspace = test_workspace("empty-default-branch");
        fs::create_dir_all(&workspace).unwrap();
        fs::write(workspace.join("graft.toml"), "schema = 1\n").unwrap();
        let config = GraftConfig::default();

        let message = run_repo_command(
            &workspace,
            &config,
            &RepoCommand::Add {
                repo_id: "demo".to_string(),
                url: "file:///tmp/demo.git".to_string(),
                default_branch: Some(" \t".to_string()),
            },
        )
        .unwrap_err()
        .to_string();

        assert!(
            message.contains("repo demo default_branch must not be empty"),
            "{message}"
        );
        assert!(
            !fs::read_to_string(workspace.join("graft.toml"))
                .unwrap()
                .contains("[repos.demo]"),
            "repo add must fail before appending an invalid repo config"
        );
        let _ = fs::remove_dir_all(workspace);
    }

    #[test]
    fn repo_add_records_remote_default_branch_when_omitted() {
        let root = test_workspace("repo-add-default-branch");
        let source = root.join("source");
        let workspace = root.join("workspace");
        fs::create_dir_all(&source).unwrap();
        fs::create_dir_all(&workspace).unwrap();
        git(&source, &["init", "-b", "trunk"]);
        fs::write(source.join("README.md"), "demo\n").unwrap();
        git(&source, &["add", "README.md"]);
        git(
            &source,
            &[
                "-c",
                "commit.gpgsign=false",
                "-c",
                "user.name=Graft Test",
                "-c",
                "user.email=graft@example.invalid",
                "commit",
                "-m",
                "initial",
            ],
        );
        let store = GraftStore::open(&workspace);
        store.init().unwrap();
        fs::write(workspace.join("properties.roto"), "").unwrap();
        crate::config::write_property_lock(&store, &BTreeMap::new()).unwrap();

        run_repo_command(
            &workspace,
            &GraftConfig::default(),
            &RepoCommand::Add {
                repo_id: "demo".to_string(),
                url: source.to_string_lossy().to_string(),
                default_branch: None,
            },
        )
        .unwrap();

        let config = fs::read_to_string(workspace.join("graft.toml")).unwrap();
        assert!(config.contains("default_branch = \"trunk\""), "{config}");
        let lock = fs::read_to_string(workspace.join("graft.lock")).unwrap();
        assert!(lock.contains("treeish = \"trunk\""), "{lock}");
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn repo_update_rejects_existing_cache_with_different_origin() {
        let root = test_workspace("repo-update-origin-drift");
        let source_a = root.join("source-a");
        let source_b = root.join("source-b");
        let workspace = root.join("workspace");
        fs::create_dir_all(&workspace).unwrap();
        init_source_repo(&source_a, "a\n");
        init_source_repo(&source_b, "b\n");
        let store = GraftStore::open(&workspace);
        store.init().unwrap();
        fs::write(workspace.join("properties.roto"), "").unwrap();
        crate::config::write_property_lock(&store, &BTreeMap::new()).unwrap();
        let source_a_url = source_a.to_string_lossy().to_string();
        let source_b_url = source_b.to_string_lossy().to_string();

        run_repo_command(
            &workspace,
            &GraftConfig::default(),
            &RepoCommand::Add {
                repo_id: "demo".to_string(),
                url: source_a_url,
                default_branch: None,
            },
        )
        .unwrap();

        let error = run_repo_command(
            &workspace,
            &config_with_repo("demo", &source_b_url, Some("main")),
            &RepoCommand::Update {
                repo: Some("demo".to_string()),
            },
        )
        .unwrap_err()
        .to_string();

        assert!(error.contains("[E_REPO_CACHE_URL_DRIFT]"), "{error}");
        assert!(error.contains("source-a"), "{error}");
        assert!(error.contains("source-b"), "{error}");
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn repo_tree_snapshot_clones_missing_cache_before_reading_locked_tree() {
        let root = test_workspace("repo-tree-snapshot-missing-cache");
        let source = root.join("source");
        let workspace = root.join("workspace");
        fs::create_dir_all(&workspace).unwrap();
        init_source_repo(&source, "a\n");
        let tree = git_stdout(&source, &["rev-parse", "HEAD^{tree}"]);
        let store = GraftStore::open(&workspace);
        store.init().unwrap();
        let config = config_with_repo("demo", &source.to_string_lossy(), Some("main"));
        let state = StateId::RepoTree(graft_core::RepoBaseState::new("demo", "main", tree));

        let snapshot = materialized_snapshot_for_state(&store, &config, &state).unwrap();

        assert!(workspace.join(".graft/repos/demo/.git").exists());
        assert!(
            snapshot
                .entries
                .iter()
                .any(|entry| entry.path == "README.md" && entry.size == 2),
            "{snapshot:?}"
        );
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn repo_tree_snapshot_rejects_existing_cache_with_different_origin() {
        let root = test_workspace("repo-tree-snapshot-origin-drift");
        let source_a = root.join("source-a");
        let source_b = root.join("source-b");
        let workspace = root.join("workspace");
        fs::create_dir_all(&workspace).unwrap();
        init_source_repo(&source_a, "a\n");
        init_source_repo(&source_b, "b\n");
        let source_a_url = source_a.to_string_lossy().to_string();
        let source_b_url = source_b.to_string_lossy().to_string();
        let tree = git_stdout(&source_a, &["rev-parse", "HEAD^{tree}"]);
        let store = GraftStore::open(&workspace);
        store.init().unwrap();
        let cache_path = config_with_repo("demo", &source_a_url, Some("main"))
            .repo_path(&workspace, "demo")
            .unwrap();
        GixBackend
            .ensure_repo(&source_a_url, &cache_path)
            .expect("seed source-a repo cache");
        let state = StateId::RepoTree(graft_core::RepoBaseState::new("demo", "main", tree));

        let error = materialized_snapshot_for_state(
            &store,
            &config_with_repo("demo", &source_b_url, Some("main")),
            &state,
        )
        .unwrap_err()
        .to_string();

        assert!(error.contains("[E_REPO_CACHE_URL_DRIFT]"), "{error}");
        assert!(error.contains("source-a"), "{error}");
        assert!(error.contains("source-b"), "{error}");
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn repo_base_resolution_uses_locked_tree_after_branch_moves() {
        let root = test_workspace("repo-base-locked-tree");
        let source = root.join("source");
        let workspace = root.join("workspace");
        fs::create_dir_all(&source).unwrap();
        fs::create_dir_all(&workspace).unwrap();
        git(&source, &["init", "-b", "main"]);
        fs::write(source.join("README.md"), "base\n").unwrap();
        git(&source, &["add", "README.md"]);
        commit(&source, "base");
        let base_tree = git_stdout(&source, &["rev-parse", "HEAD^{tree}"]);

        let store = GraftStore::open(&workspace);
        store.init().unwrap();
        crate::config::write_property_lock(&store, &BTreeMap::new()).unwrap();
        let url = source.to_string_lossy().to_string();
        crate::config::write_repo_lock_entry(&store, "demo", &url, "main", &base_tree).unwrap();

        fs::write(source.join("README.md"), "updated\n").unwrap();
        git(&source, &["add", "README.md"]);
        commit(&source, "update");
        let moved_tree = git_stdout(&source, &["rev-parse", "HEAD^{tree}"]);
        assert_ne!(base_tree, moved_tree);

        let state = resolve_base_state(
            &store,
            &config_with_repo("demo", &url, Some("main")),
            "repo:demo@main",
        )
        .unwrap();

        let StateId::RepoTree(repo) = state else {
            panic!("expected repo tree state");
        };
        assert_eq!(repo.repo_id, "demo");
        assert_eq!(repo.treeish, "main");
        assert_eq!(repo.resolved_tree_oid, base_tree);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn repo_base_resolution_rejects_unlocked_treeish() {
        let workspace = test_workspace("repo-base-unlocked-treeish");
        fs::create_dir_all(&workspace).unwrap();
        let store = GraftStore::open(&workspace);
        store.init().unwrap();
        crate::config::write_property_lock(&store, &BTreeMap::new()).unwrap();
        crate::config::write_repo_lock_entry(
            &store,
            "demo",
            "https://example.invalid/demo",
            "main",
            "tree-locked",
        )
        .unwrap();

        let error = resolve_base_state(
            &store,
            &config_with_repo("demo", "https://example.invalid/demo", Some("main")),
            "repo:demo@dev",
        )
        .unwrap_err()
        .to_string();

        assert!(error.contains("[E_REPO_LOCK_DRIFT]"), "{error}");
        assert!(
            error.contains("repo base `demo@dev` is not locked"),
            "{error}"
        );
        let _ = fs::remove_dir_all(workspace);
    }

    #[test]
    fn repo_base_resolution_rejects_url_drift() {
        let workspace = test_workspace("repo-base-url-drift");
        fs::create_dir_all(&workspace).unwrap();
        let store = GraftStore::open(&workspace);
        store.init().unwrap();
        crate::config::write_property_lock(&store, &BTreeMap::new()).unwrap();
        crate::config::write_repo_lock_entry(
            &store,
            "demo",
            "https://example.invalid/old",
            "main",
            "tree-locked",
        )
        .unwrap();

        let error = resolve_base_state(
            &store,
            &config_with_repo("demo", "https://example.invalid/new", Some("main")),
            "repo:demo@main",
        )
        .unwrap_err()
        .to_string();

        assert!(error.contains("[E_REPO_LOCK_DRIFT]"), "{error}");
        assert!(
            error.contains("url in graft.lock does not match"),
            "{error}"
        );
        let _ = fs::remove_dir_all(workspace);
    }

    #[test]
    fn git_base_resolution_preserves_git_source_error() {
        let workspace = test_workspace("git-base-source");
        fs::create_dir_all(&workspace).unwrap();
        let store = GraftStore::open(&workspace);

        let error = resolve_base_state(&store, &GraftConfig::default(), "HEAD")
            .unwrap_err()
            .to_string();

        assert!(error.contains("[B001]"), "{error}");
        assert!(error.contains("source:"), "{error}");
        assert!(error.contains("not a git repository"), "{error}");
        let _ = fs::remove_dir_all(workspace);
    }

    fn test_workspace(name: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!(
            "graft-runtime-repo-{name}-{}-{nanos}",
            std::process::id()
        ))
    }

    fn config_with_repo(repo_id: &str, url: &str, default_branch: Option<&str>) -> GraftConfig {
        let mut repos = BTreeMap::new();
        repos.insert(
            repo_id.to_string(),
            RepoConfig {
                url: url.to_string(),
                default_branch: default_branch.map(str::to_string),
            },
        );
        GraftConfig {
            repos,
            ..GraftConfig::default()
        }
    }

    fn commit(path: &Path, message: &str) {
        git(
            path,
            &[
                "-c",
                "commit.gpgsign=false",
                "-c",
                "user.name=Graft Test",
                "-c",
                "user.email=graft@example.invalid",
                "commit",
                "-m",
                message,
            ],
        );
    }

    fn init_source_repo(path: &Path, readme: &str) {
        fs::create_dir_all(path).unwrap();
        git(path, &["init", "-b", "main"]);
        fs::write(path.join("README.md"), readme).unwrap();
        git(path, &["add", "README.md"]);
        commit(path, "initial");
    }

    fn git(path: &Path, args: &[&str]) {
        let status = std::process::Command::new("git")
            .arg("-C")
            .arg(path)
            .args(args)
            .status()
            .unwrap();
        assert!(status.success(), "git {args:?} failed with {status}");
    }

    fn git_stdout(path: &Path, args: &[&str]) -> String {
        let output = std::process::Command::new("git")
            .arg("-C")
            .arg(path)
            .args(args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "git {args:?} failed with {}",
            output.status
        );
        String::from_utf8(output.stdout).unwrap().trim().to_string()
    }
}
