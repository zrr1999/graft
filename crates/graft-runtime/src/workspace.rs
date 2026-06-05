use std::collections::BTreeSet;
use std::fs;
use std::path::Path;
use std::process::Command as ProcessCommand;

use anyhow::{Context, Result, bail};
use graft_client::{DaemonSocketState, daemon_socket_path, daemon_socket_state};
use graft_core::blake3_hex_digest;
use graft_store::{
    GraftStore, InitOutcome, Registry, RegistryStore, RepoPathsRecord, RouteRecord, StoreError,
    WorkspaceDiscovery, WorkspaceKind, WorkspaceRecord, local_workspace_id_for_root,
    normalize_workspace_path,
};
use time::{OffsetDateTime, format_description::well_known::Rfc3339};

use crate::WorkspaceCommand;
use crate::config::{load_property_defs, read_property_lock, write_property_lock};
use crate::ensure_workspace_initialized;
use crate::view::{
    CommandEnvelope, CommandView, DaemonView, GcRegistryView, GcView, GcWorkspaceView, PsView,
    RegistryOverviewView, WorkspaceSummaryView,
};

pub(crate) fn modernize_legacy_gc_apply_message(
    mut envelope: CommandEnvelope,
    derived_only: bool,
) -> CommandEnvelope {
    let Some(message) = envelope.message.as_deref() else {
        return envelope;
    };
    if !message.starts_with("gc dry_run=false;") {
        return envelope;
    }
    let numbers = parse_usize_fields(message);
    envelope.view = if derived_only {
        numbers
            .first()
            .map(|count| gc_derived_only_view(true, *count))
    } else if message.contains("workspace objects skipped") {
        (numbers.len() >= 4).then(|| {
            gc_registry_only_view(
                true,
                RegistryGcReport {
                    missing_workspaces: numbers[1],
                    stale_routes: numbers[2],
                    missing_repo_paths: numbers[3],
                },
            )
        })
    } else if numbers.len() >= 4 {
        Some(gc_workspace_view(
            true,
            numbers[1],
            numbers[2],
            numbers[3],
            RegistryGcReport {
                missing_workspaces: numbers.get(5).copied().unwrap_or(0),
                stale_routes: numbers.get(6).copied().unwrap_or(0),
                missing_repo_paths: numbers.get(7).copied().unwrap_or(0),
            },
        ))
    } else {
        None
    };
    if envelope.view.is_some() {
        envelope.message = None;
    }
    envelope
}

fn parse_usize_fields(message: &str) -> Vec<usize> {
    message
        .split(|character: char| !character.is_ascii_digit())
        .filter(|part| !part.is_empty())
        .filter_map(|part| part.parse().ok())
        .collect()
}

pub(crate) fn gc_apply_daemon_argv(workspace_root: &Path, derived_only: bool) -> Vec<String> {
    let mut argv = vec![
        "graft".to_string(),
        "--cwd".to_string(),
        workspace_root.display().to_string(),
        "gc".to_string(),
        "--apply".to_string(),
    ];
    if derived_only {
        argv.push("--derived-only".to_string());
    }
    argv
}

pub(crate) fn run_init_command(store: &GraftStore, register_only: bool) -> Result<CommandEnvelope> {
    if register_only && !store.is_initialized() {
        bail!(
            "[E_NO_CONFIG] cannot --register-only {}; graft.toml is missing",
            store.paths().workspace().display()
        );
    }

    let (outcome, lock_created) = if register_only {
        (Default::default(), false)
    } else {
        init_workspace_files(store)?
    };
    if !register_only {
        let registry_record = register_local_workspace(store)?;
        let message = if outcome.changed() || lock_created {
            format!(
                "initialized .graft, graft.toml, properties.roto and graft.lock; registered {}",
                registry_record.id
            )
        } else {
            format!("already initialized; registered {}", registry_record.id)
        };
        return Ok(CommandEnvelope {
            message: Some(message),
            cache_changed: outcome.layout_created,
            registry_changed: true,
            ..CommandEnvelope::ok()
        });
    }

    let registry_record = register_local_workspace(store)?;
    Ok(CommandEnvelope {
        message: Some(format!(
            "registered existing workspace {} at {}",
            registry_record.id,
            registry_record.root.display()
        )),
        registry_changed: true,
        ..CommandEnvelope::ok()
    })
}

pub(crate) fn init_workspace_files(store: &GraftStore) -> Result<(InitOutcome, bool)> {
    let outcome = store.init()?;
    let defs = load_property_defs(store)?;
    let lock_created = read_property_lock(store)?.is_none();
    write_property_lock(store, &defs)?;
    Ok((outcome, lock_created))
}

fn register_local_workspace(store: &GraftStore) -> Result<graft_store::WorkspaceRecord> {
    let root = store.paths().workspace();
    let id = local_workspace_id_for_root(root);
    Ok(RegistryStore::from_env().ensure_workspace(id, WorkspaceKind::Local, root)?)
}

pub(crate) fn run_attach_command(
    cwd: &Path,
    workspace: Option<&str>,
    status: bool,
) -> Result<CommandEnvelope> {
    if status {
        return attach_status(cwd);
    }

    let cwd = normalize_workspace_path(cwd);
    let registry_store = RegistryStore::from_env();
    let workspace_id = workspace.unwrap_or(graft_store::DEFAULT_WORKSPACE_ID);
    let default_root = prepare_default_workspace_if_needed(&registry_store, workspace_id)?;
    let origin = git_origin(&cwd)?;
    let repo_registration = origin.as_ref().map(|origin| {
        (
            repo_id_for_url(&origin.url),
            origin.root.clone(),
            origin.url.clone(),
        )
    });

    registry_store.with_mut(|registry| {
        ensure_workspace_record_in_registry(registry, workspace_id, default_root.as_deref())?;
        upsert_route_in_registry(registry, &cwd, workspace_id)?;
        if let Some((repo_id, root, _url)) = &repo_registration {
            upsert_repo_path_in_registry(registry, repo_id, root)?;
        }
        Ok(())
    })?;

    let mut details = vec![format!("attached {} -> {workspace_id}", cwd.display())];
    if let Some((repo_id, _root, url)) = repo_registration {
        details.push(format!("registered repo path {repo_id} ({url})"));
    } else {
        details.push("no git origin detected; route only".to_string());
    }

    Ok(CommandEnvelope {
        message: Some(details.join("; ")),
        registry_changed: true,
        ..CommandEnvelope::ok()
    })
}

pub(crate) fn run_detach_command(cwd: &Path) -> Result<CommandEnvelope> {
    let cwd = normalize_workspace_path(cwd);
    let removed = RegistryStore::from_env().remove_route(&cwd)?;
    Ok(CommandEnvelope {
        message: Some(if removed {
            format!("detached {}", cwd.display())
        } else {
            format!("no route for {}", cwd.display())
        }),
        registry_changed: removed,
        ..CommandEnvelope::ok()
    })
}

fn attach_status(cwd: &Path) -> Result<CommandEnvelope> {
    let cwd = normalize_workspace_path(cwd);
    let registry = RegistryStore::from_env();
    let route = registry.lookup_route_for_cwd(&cwd)?;
    let git = git_origin(&cwd)?;
    let mut lines = vec![format!("cwd\t{}", cwd.display())];
    if let Some(route) = route {
        lines.push(format!(
            "route\t{} -> {}",
            route.cwd.display(),
            route.workspace
        ));
    } else {
        lines.push("route\t<none>".to_string());
    }
    if let Some(origin) = git {
        lines.push(format!("git_origin\t{}", origin.url));
        lines.push(format!("git_root\t{}", origin.root.display()));
        lines.push(format!("repo_id\t{}", repo_id_for_url(&origin.url)));
    } else {
        lines.push("git_origin\t<none>".to_string());
    }
    Ok(CommandEnvelope {
        message: Some(lines.join("\n")),
        ..CommandEnvelope::ok()
    })
}

pub(crate) fn run_ps_command() -> Result<CommandEnvelope> {
    let registry_store = RegistryStore::from_env();
    let registry = registry_store.load()?;
    let socket = daemon_socket_path()?;
    let socket_state = daemon_socket_state(&socket)?;
    let pid_path = daemon_socket_run_dir(&socket)?.join("daemon.pid");
    let pid = fs::read_to_string(&pid_path)
        .ok()
        .map(|pid| pid.trim().to_string());

    let mut visible_registry = registry.clone();
    let hidden_report = prune_missing_registry_records(&mut visible_registry);
    let workspaces = visible_registry
        .workspaces
        .iter()
        .map(|workspace| WorkspaceSummaryView {
            id: workspace.id.clone(),
            kind: format!("{:?}", workspace.kind),
            root: workspace.root.display().to_string(),
        })
        .collect();

    Ok(CommandEnvelope {
        view: Some(CommandView::Ps(PsView {
            daemon: DaemonView {
                graft_home: registry_store.home().display().to_string(),
                socket: socket.display().to_string(),
                socket_state: daemon_socket_state_label(socket_state).to_string(),
                socket_exists: socket.exists(),
                pid_file: pid_path.display().to_string(),
                pid,
            },
            registry: RegistryOverviewView {
                workspaces: visible_registry.workspaces.len(),
                workspaces_hidden_missing: hidden_report.missing_workspaces,
                routes: visible_registry.routes.len(),
                routes_hidden_stale: hidden_report.stale_routes,
                repo_paths: visible_registry.repo_paths.len(),
                repo_paths_hidden_missing: hidden_report.missing_repo_paths,
            },
            workspaces,
        })),
        ..CommandEnvelope::ok()
    })
}

pub(crate) fn daemon_socket_run_dir(socket: &Path) -> Result<&Path> {
    let Some(parent) = socket
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    else {
        bail!(
            "[E_SOCKET_PARENT_REQUIRED] graft daemon socket path must include an explicit parent directory: {}",
            socket.display()
        );
    };
    Ok(parent)
}

pub(crate) fn run_doctor_command(rebuild_registry: bool) -> Result<CommandEnvelope> {
    let registry_store = RegistryStore::from_env();
    if rebuild_registry {
        rebuild_registry_from_home(&registry_store)?;
    }
    let registry = registry_store.load()?;
    let mut problems = Vec::new();
    for workspace in &registry.workspaces {
        if !workspace.root.exists() {
            problems.push(format!(
                "missing workspace root: {} -> {}",
                workspace.id,
                workspace.root.display()
            ));
        }
    }
    for route in &registry.routes {
        if !route.cwd.exists() {
            problems.push(format!("missing route cwd: {}", route.cwd.display()));
        }
        if !registry
            .workspaces
            .iter()
            .any(|workspace| workspace.id == route.workspace)
        {
            problems.push(format!(
                "route points to unknown workspace: {} -> {}",
                route.cwd.display(),
                route.workspace
            ));
        }
    }
    for repo in &registry.repo_paths {
        for path in &repo.paths {
            if !path.exists() {
                problems.push(format!(
                    "missing repo path: {} -> {}",
                    repo.repo_id,
                    path.display()
                ));
            }
        }
    }

    let mut lines = vec![
        format!("registry\t{}", registry_store.registry_path().display()),
        format!("workspaces\t{}", registry.workspaces.len()),
        format!("routes\t{}", registry.routes.len()),
        format!("repo_paths\t{}", registry.repo_paths.len()),
    ];
    if rebuild_registry {
        lines.push("rebuilt\ttrue".to_string());
    }
    if problems.is_empty() {
        lines.push("status\tok".to_string());
    } else {
        lines.push(format!("status\t{} problem(s)", problems.len()));
        lines.extend(
            problems
                .into_iter()
                .map(|problem| format!("problem\t{problem}")),
        );
    }
    Ok(CommandEnvelope {
        message: Some(lines.join("\n")),
        registry_changed: rebuild_registry,
        ..CommandEnvelope::ok()
    })
}

fn rebuild_registry_from_home(registry_store: &RegistryStore) -> Result<()> {
    let mut registry = match registry_store.load() {
        Ok(registry) => registry,
        Err(StoreError::TomlDeserialize(_) | StoreError::InvalidRegistrySchema { .. }) => {
            Registry::default()
        }
        Err(error) => return Err(error.into()),
    };
    registry.workspaces.clear();
    let workspaces_dir = registry_store.home().join("workspaces");
    if workspaces_dir.exists() {
        for entry in fs::read_dir(&workspaces_dir)? {
            let path = entry?.path();
            if !path.is_dir() || !path.join(".graft").is_dir() || !path.join("graft.toml").exists()
            {
                continue;
            }
            let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
                continue;
            };
            let workspace_id = if name == "default" {
                graft_store::DEFAULT_WORKSPACE_ID.to_string()
            } else {
                format!("ws:{name}")
            };
            registry.workspaces.push(WorkspaceRecord {
                id: workspace_id,
                kind: WorkspaceKind::System,
                root: normalize_workspace_path(&path),
                created_at: OffsetDateTime::now_utc().format(&Rfc3339)?,
            });
        }
    }
    registry_store.replace(&registry)?;
    Ok(())
}

fn prepare_default_workspace_if_needed(
    registry_store: &RegistryStore,
    workspace_id: &str,
) -> Result<Option<std::path::PathBuf>> {
    if workspace_id != graft_store::DEFAULT_WORKSPACE_ID {
        return Ok(None);
    }
    let root = graft_store::default_workspace_root();
    if registry_store.get_workspace(workspace_id)?.is_none() {
        init_workspace_files(&GraftStore::open(&root))?;
    }
    Ok(Some(root))
}

fn ensure_workspace_record_in_registry(
    registry: &mut Registry,
    workspace_id: &str,
    default_root: Option<&Path>,
) -> std::result::Result<(), StoreError> {
    if registry
        .workspaces
        .iter()
        .any(|workspace| workspace.id == workspace_id)
    {
        return Ok(());
    }
    if workspace_id != graft_store::DEFAULT_WORKSPACE_ID {
        return Err(StoreError::InvalidWorkspace(format!(
            "[E_UNKNOWN_WORKSPACE] workspace {workspace_id} is not registered"
        )));
    }
    let Some(root) = default_root else {
        return Err(StoreError::InvalidWorkspace(
            "[E_UNKNOWN_WORKSPACE] default workspace root was not prepared".to_string(),
        ));
    };
    registry.workspaces.push(WorkspaceRecord {
        id: workspace_id.to_string(),
        kind: WorkspaceKind::System,
        root: normalize_workspace_path(root),
        created_at: now_rfc3339()?,
    });
    registry.workspaces.sort_by(|a, b| a.id.cmp(&b.id));
    Ok(())
}

fn upsert_route_in_registry(
    registry: &mut Registry,
    cwd: &Path,
    workspace_id: &str,
) -> std::result::Result<(), StoreError> {
    if let Some(existing) = registry.routes.iter_mut().find(|route| route.cwd == cwd) {
        existing.workspace = workspace_id.to_string();
        return Ok(());
    }
    registry.routes.push(RouteRecord {
        cwd: cwd.to_path_buf(),
        workspace: workspace_id.to_string(),
        created_at: now_rfc3339()?,
    });
    registry.routes.sort_by(|a, b| a.cwd.cmp(&b.cwd));
    Ok(())
}

fn upsert_repo_path_in_registry(
    registry: &mut Registry,
    repo_id: &str,
    path: &Path,
) -> std::result::Result<(), StoreError> {
    if let Some(existing) = registry
        .repo_paths
        .iter_mut()
        .find(|record| record.repo_id == repo_id)
    {
        if !existing.paths.iter().any(|existing| existing == path) {
            existing.paths.push(path.to_path_buf());
            existing.paths.sort();
        }
        existing.last_seen_at = now_rfc3339()?;
        return Ok(());
    }
    registry.repo_paths.push(RepoPathsRecord {
        repo_id: repo_id.to_string(),
        paths: vec![path.to_path_buf()],
        last_seen_at: now_rfc3339()?,
    });
    registry
        .repo_paths
        .sort_by(|a, b| a.repo_id.cmp(&b.repo_id));
    Ok(())
}

fn now_rfc3339() -> std::result::Result<String, StoreError> {
    Ok(OffsetDateTime::now_utc().format(&Rfc3339)?)
}

#[derive(Debug)]
struct GitOrigin {
    root: std::path::PathBuf,
    url: String,
}

fn git_origin(cwd: &Path) -> Result<Option<GitOrigin>> {
    let Some(root) = git_worktree_root(cwd)? else {
        return Ok(None);
    };
    let output = ProcessCommand::new("git")
        .arg("-C")
        .arg(&root)
        .arg("config")
        .arg("--get")
        .arg("remote.origin.url")
        .output()
        .with_context(|| format!("inspect git origin in {}", root.display()))?;
    if output.status.success() {
        return Ok(
            git_origin_url_from_stdout(&root, output.stdout)?.map(|url| GitOrigin { root, url })
        );
    }
    if output.status.code() == Some(1) && output.stdout.is_empty() && output.stderr.is_empty() {
        return Ok(None);
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    bail!(
        "[E_GIT_ORIGIN_LOOKUP_FAILED] failed to inspect git remote.origin.url in {} (status {}): {}",
        root.display(),
        output.status,
        stderr.trim()
    );
}

#[cfg(test)]
pub(crate) fn git_origin_url(cwd: &Path) -> Result<Option<String>> {
    Ok(git_origin(cwd)?.map(|origin| origin.url))
}

fn git_worktree_root(cwd: &Path) -> Result<Option<std::path::PathBuf>> {
    if !cwd.exists() {
        return Ok(None);
    }
    let Some(marker_root) = cwd.ancestors().find(|path| path.join(".git").exists()) else {
        return Ok(None);
    };
    let marker_root = normalize_workspace_path(marker_root);
    let output = ProcessCommand::new("git")
        .arg("-C")
        .arg(cwd)
        .arg("rev-parse")
        .arg("--show-toplevel")
        .output()
        .with_context(|| format!("inspect git worktree root in {}", cwd.display()))?;
    if output.status.success() {
        let root = String::from_utf8(output.stdout).with_context(|| {
            format!(
                "[E_NON_UTF8_GIT_ROOT] git worktree root in {} is not valid UTF-8",
                cwd.display()
            )
        })?;
        let root = root.trim();
        if root.is_empty() {
            bail!(
                "[E_GIT_ROOT_LOOKUP_FAILED] git returned an empty worktree root for {}",
                cwd.display()
            );
        }
        return Ok(Some(normalize_workspace_path(Path::new(root))));
    }
    Ok(Some(marker_root))
}

pub(crate) fn git_origin_url_from_stdout(cwd: &Path, stdout: Vec<u8>) -> Result<Option<String>> {
    let stdout = trim_git_line_ending(&stdout);
    if stdout.is_empty() {
        return Ok(None);
    }
    let url = String::from_utf8(stdout.to_vec())
        .with_context(|| {
            format!(
                "[E_NON_UTF8_GIT_ORIGIN] git remote.origin.url in {} is not valid UTF-8",
                cwd.display()
            )
        })?
        .to_string();
    Ok(Some(url))
}

fn trim_git_line_ending(bytes: &[u8]) -> &[u8] {
    let mut end = bytes.len();
    if end > 0 && bytes[end - 1] == b'\n' {
        end -= 1;
        if end > 0 && bytes[end - 1] == b'\r' {
            end -= 1;
        }
    }
    &bytes[..end]
}

pub(crate) fn repo_id_for_url(url: &str) -> String {
    let canonical = url.trim_end_matches(".git").to_ascii_lowercase();
    format!("repo:{}", &blake3_hex_digest(canonical.as_bytes())[..12])
}

pub(crate) fn run_workspace_command(
    command: &WorkspaceCommand,
    cwd: &Path,
    store: &GraftStore,
) -> Result<CommandEnvelope> {
    match command {
        WorkspaceCommand::Init { register_only } => run_init_command(store, *register_only),
        WorkspaceCommand::Status => workspace_status(cwd),
        WorkspaceCommand::Attach { workspace, status } => {
            run_attach_command(cwd, workspace.as_deref(), *status)
        }
        WorkspaceCommand::Detach => run_detach_command(cwd),
        WorkspaceCommand::Ps => run_ps_command(),
        WorkspaceCommand::Doctor { rebuild_registry } => run_doctor_command(*rebuild_registry),
        WorkspaceCommand::Gc {
            apply,
            derived_only,
        } => run_gc(store, *apply, *derived_only),
    }
}

pub(crate) fn run_gc(
    store: &GraftStore,
    apply: bool,
    derived_only: bool,
) -> Result<CommandEnvelope> {
    if derived_only {
        ensure_workspace_initialized(store)?;
        store.init_storage()?;
        let derived = store.list_evidence_body_ids()?;
        if apply {
            for id in &derived {
                remove_gc_file(&store.paths().object_evidence().join(format!("{id}.json")))?;
            }
        }
        return Ok(CommandEnvelope {
            view: Some(gc_derived_only_view(apply, derived.len())),
            registry_changed: false,
            cache_changed: apply && !derived.is_empty(),
            ..CommandEnvelope::ok()
        });
    }

    let registry_report = registry_gc(apply)?;
    let registry_stale_count = registry_report.total();
    if !store.is_initialized() {
        return Ok(CommandEnvelope {
            view: Some(gc_registry_only_view(apply, registry_report)),
            registry_changed: apply && registry_stale_count > 0,
            ..CommandEnvelope::ok()
        });
    }

    store.init_storage()?;
    let mut reachable_evidence = BTreeSet::new();
    for candidate in store.list_candidates()? {
        for id in store.candidate_evidence_index(candidate.id.as_str())? {
            reachable_evidence.insert(id);
        }
    }
    for patch in store.list_patches()? {
        for id in store.patch_evidence_index(patch.id.as_str())? {
            reachable_evidence.insert(id);
        }
    }

    let mut orphan_evidence = Vec::new();
    for id in store.list_evidence_body_ids()? {
        if !reachable_evidence.contains(&id) {
            orphan_evidence.push(id);
        }
    }

    let mut orphan_candidate_indexes = Vec::new();
    let live_candidates = store
        .list_candidates()?
        .into_iter()
        .map(|candidate| candidate.id.to_string())
        .collect::<BTreeSet<_>>();
    for id in store.list_candidate_evidence_ref_owners()? {
        if !live_candidates.contains(&id) {
            orphan_candidate_indexes.push(id);
        }
    }

    let mut orphan_patch_indexes = Vec::new();
    let live_patches = store
        .list_patches()?
        .into_iter()
        .map(|patch| patch.id.to_string())
        .collect::<BTreeSet<_>>();
    for id in store.list_patch_evidence_ref_owners()? {
        if !live_patches.contains(&id) {
            orphan_patch_indexes.push(id);
        }
    }

    let orphan_count =
        orphan_evidence.len() + orphan_candidate_indexes.len() + orphan_patch_indexes.len();
    if apply {
        for id in &orphan_evidence {
            remove_gc_file(&store.paths().object_evidence().join(format!("{id}.json")))?;
        }
        for id in &orphan_candidate_indexes {
            remove_gc_file(
                &store
                    .paths()
                    .object_candidate_evidence_index()
                    .join(format!("{id}.json")),
            )?;
        }
        for id in &orphan_patch_indexes {
            remove_gc_file(
                &store
                    .paths()
                    .object_patch_evidence_index()
                    .join(format!("{id}.json")),
            )?;
        }
    }

    Ok(CommandEnvelope {
        view: Some(gc_workspace_view(
            apply,
            orphan_evidence.len(),
            orphan_candidate_indexes.len(),
            orphan_patch_indexes.len(),
            registry_report,
        )),
        registry_changed: apply && registry_stale_count > 0,
        cache_changed: apply && orphan_count > 0,
        ..CommandEnvelope::ok()
    })
}

fn gc_derived_only_view(apply: bool, evidence_bodies: usize) -> CommandView {
    CommandView::Gc(GcView {
        dry_run: !apply,
        workspace: GcWorkspaceView::DerivedEvidenceBodies {
            evidence_bodies_before: evidence_bodies,
            evidence_bodies_selected: evidence_bodies,
        },
        registry: None,
        apply_hint: gc_apply_hint(apply),
    })
}

fn gc_registry_only_view(apply: bool, registry_report: RegistryGcReport) -> CommandView {
    CommandView::Gc(GcView {
        dry_run: !apply,
        workspace: GcWorkspaceView::RegistryOnly {
            workspace_objects: "skipped (no initialized workspace)".to_string(),
        },
        registry: Some(registry_report.into_view()),
        apply_hint: gc_apply_hint(apply),
    })
}

fn gc_workspace_view(
    apply: bool,
    evidence_bodies: usize,
    candidate_evidence_indexes: usize,
    patch_evidence_indexes: usize,
    registry_report: RegistryGcReport,
) -> CommandView {
    let orphan_objects = evidence_bodies + candidate_evidence_indexes + patch_evidence_indexes;
    CommandView::Gc(GcView {
        dry_run: !apply,
        workspace: GcWorkspaceView::Workspace {
            orphan_objects_before: orphan_objects,
            orphan_evidence_bodies: evidence_bodies,
            orphan_candidate_evidence_indexes: candidate_evidence_indexes,
            orphan_patch_evidence_indexes: patch_evidence_indexes,
            orphan_objects_selected: orphan_objects,
        },
        registry: Some(registry_report.into_view()),
        apply_hint: gc_apply_hint(apply),
    })
}

fn gc_apply_hint(apply: bool) -> Option<String> {
    (!apply).then(|| "answer y at the prompt or rerun with --apply".to_string())
}

#[derive(Clone, Copy, Debug, Default)]
struct RegistryGcReport {
    missing_workspaces: usize,
    stale_routes: usize,
    missing_repo_paths: usize,
}

impl RegistryGcReport {
    fn total(self) -> usize {
        self.missing_workspaces + self.stale_routes + self.missing_repo_paths
    }

    fn into_view(self) -> GcRegistryView {
        GcRegistryView {
            stale_registry_records_before: self.total(),
            missing_workspaces: self.missing_workspaces,
            stale_routes: self.stale_routes,
            missing_repo_paths: self.missing_repo_paths,
            stale_registry_records_selected: self.total(),
        }
    }
}

fn registry_gc(apply: bool) -> Result<RegistryGcReport> {
    let registry_store = RegistryStore::from_env();
    if apply {
        Ok(registry_store.with_mut(|registry| Ok(prune_missing_registry_records(registry)))?)
    } else {
        let mut registry = registry_store.load()?;
        Ok(prune_missing_registry_records(&mut registry))
    }
}

fn prune_missing_registry_records(registry: &mut Registry) -> RegistryGcReport {
    let before_workspaces = registry.workspaces.len();
    registry
        .workspaces
        .retain(|workspace| workspace.root.exists());
    let missing_workspaces = before_workspaces - registry.workspaces.len();
    let live_workspaces = registry
        .workspaces
        .iter()
        .map(|workspace| workspace.id.clone())
        .collect::<BTreeSet<_>>();

    let before_routes = registry.routes.len();
    registry
        .routes
        .retain(|route| route.cwd.exists() && live_workspaces.contains(&route.workspace));
    let stale_routes = before_routes - registry.routes.len();

    let mut missing_repo_paths = 0;
    for repo in &mut registry.repo_paths {
        let before_paths = repo.paths.len();
        repo.paths.retain(|path| path.exists());
        missing_repo_paths += before_paths - repo.paths.len();
    }
    registry.repo_paths.retain(|repo| !repo.paths.is_empty());

    RegistryGcReport {
        missing_workspaces,
        stale_routes,
        missing_repo_paths,
    }
}

fn remove_gc_file(path: &Path) -> Result<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error).with_context(|| format!("remove gc object {}", path.display())),
    }
}

pub(crate) fn workspace_status(cwd: &Path) -> Result<CommandEnvelope> {
    let mut lines = attach_status(cwd)?
        .message
        .unwrap_or_default()
        .lines()
        .map(str::to_string)
        .collect::<Vec<_>>();
    match WorkspaceDiscovery::from_env().discover(cwd) {
        Ok(location) => {
            lines.push(format!("workspace\t{}", location.root().display()));
            lines.push(format!(
                "workspace_id\t{}",
                location.id().unwrap_or("<unregistered>")
            ));
        }
        Err(StoreError::NoWorkspace { .. }) => {
            lines.push("workspace\t<none>".to_string());
            lines.push("workspace_id\t<none>".to_string());
        }
        Err(error) => return Err(error.into()),
    }
    let socket = daemon_socket_path()?;
    let daemon_state = daemon_socket_state(&socket)?;
    let pid_path = daemon_socket_run_dir(&socket)?.join("daemon.pid");
    lines.push(format!("daemon_socket\t{}", socket.display()));
    lines.push(format!(
        "daemon_state\t{}",
        daemon_socket_state_label(daemon_state)
    ));
    lines.push(format!("daemon_pid_file\t{}", pid_path.display()));
    if let Ok(pid) = fs::read_to_string(&pid_path) {
        lines.push(format!("daemon_pid\t{}", pid.trim()));
    }
    Ok(CommandEnvelope {
        message: Some(lines.join("\n")),
        ..CommandEnvelope::ok()
    })
}

fn daemon_socket_state_label(state: DaemonSocketState) -> &'static str {
    match state {
        DaemonSocketState::Missing => "missing",
        DaemonSocketState::Live => "live",
        DaemonSocketState::Stale => "stale",
    }
}
