use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsStr;
use std::fs;
use std::io::Write;
use std::path::{Component, Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use graft_core::{
    Action, ActionId, ApplicationId, ApplicationRecord, ApplicationRef, CandidateId, Change,
    Constraint, ConstraintDef, EvidenceRecord, GraftCandidate, MaterializedApplication, PatchId,
    PatchRecord, PatchRelation, Plan, PlanId, PromotionRecord, StateId, TreeEntry, TreeSnapshot,
    action_id, application_id, evidence_id, validate_application_integrity,
};

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ResolvedApplication {
    pub record: ApplicationRecord,
    pub change: Change,
    pub action: Action,
}
use rusqlite::Connection;

pub mod discovery;
pub mod lock;
pub mod registry;
pub use discovery::{
    DEFAULT_WORKSPACE_ID, WorkspaceDiscovery, WorkspaceLocation, WorkspaceSource,
    default_workspace_root, local_workspace_id_for_root,
};
pub use lock::WriteLock;
pub use registry::{
    Registry, RegistryStore, RepoPathsRecord, RouteRecord, WorkspaceKind, WorkspaceRecord,
    graft_home_from_env,
};

mod paths;
pub use paths::{GraftPaths, normalize_workspace_path};
mod evidence;
mod index;
mod objects;
mod records;
mod virtual_tree;

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("sqlite error: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("toml deserialize error: {0}")]
    TomlDeserialize(#[from] toml::de::Error),
    #[error("toml serialize error: {0}")]
    TomlSerialize(#[from] toml::ser::Error),
    #[error("time format error: {0}")]
    TimeFormat(#[from] time::error::Format),
    #[error("core error: {0}")]
    Core(#[from] graft_core::CoreError),
    #[error("invalid stored path escapes materialized tree: {0}")]
    InvalidPath(String),
    #[error("blob hash mismatch: expected {expected}, got {actual}")]
    BlobHashMismatch { expected: String, actual: String },
    #[error("virtual path not found: {0}")]
    VirtualPathNotFound(String),
    #[error("virtual path is a directory: {0}")]
    VirtualPathIsDirectory(String),
    #[error("unsupported virtual base state: {0}")]
    UnsupportedVirtualBase(String),
    #[error("invalid workspace: {0}")]
    InvalidWorkspace(String),
    #[error("[E_REGISTRY_SCHEMA] registry.toml schema {found} is not supported")]
    InvalidRegistrySchema { found: u32 },
    #[error(
        "[E_NO_WORKSPACE] no Graft workspace is attached for cwd {cwd}; run `graft init`, `graft attach`, or set GRAFT_WORKSPACE"
    )]
    NoWorkspace { cwd: PathBuf },
    #[error("invalid evidence index {path}: {message}")]
    InvalidEvidenceIndex { path: PathBuf, message: String },
    #[error("invalid store write path: {0}")]
    InvalidStoreWritePath(PathBuf),
    #[error("invalid store object path {path}: {message}")]
    InvalidStoreObjectPath { path: PathBuf, message: String },
    #[error("invalid snapshot path {path}: {message}")]
    InvalidSnapshotPath { path: PathBuf, message: String },
    #[error("store object id mismatch at {path}: expected {expected}, got {actual}")]
    StoreObjectIdMismatch {
        path: PathBuf,
        expected: String,
        actual: String,
    },
    #[error("[E_UNSUPPORTED_STORE_SCHEMA] unsupported legacy store schema in {path}: {message}")]
    UnsupportedStoreSchema { path: PathBuf, message: String },
    #[error("failed to publish atomic write at {path}: {message}")]
    AtomicWrite { path: PathBuf, message: String },
    #[error("invalid materialize destination: {0}")]
    InvalidMaterializeDestination(PathBuf),
    #[error("failed to publish materialized tree at {path}: {message}")]
    MaterializePublish { path: PathBuf, message: String },
    #[error(
        "another graft writer holds the lock at {} - only one graftd may write `.graft/` at a time",
        path.display()
    )]
    Locked { path: PathBuf },
}

pub type Result<T> = std::result::Result<T, StoreError>;

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum VirtualBaseRef {
    Empty,
    Tree(String),
    Candidate(CandidateId),
    Patch(PatchId),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct VirtualFile {
    pub path: String,
    pub hash: String,
    pub size: u64,
    pub bytes: Vec<u8>,
}

#[derive(Clone, Debug, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct EvidenceRefsRecord {
    pub owner: String,
    pub evidence: Vec<String>,
    pub updated_at: Option<String>,
}

#[derive(Clone, Debug)]
pub struct GraftStore {
    paths: GraftPaths,
}

/// Result of [`GraftStore::init`]: which top-level paths had to be created.
///
/// `layout_created` covers the on-disk `.graft/` directory tree;
/// `config_created` covers the user-facing `graft.toml` file.
/// Both fields are false when init is a no-op (idempotent re-run), so callers
/// can render an honest "already initialized" message instead of pretending
/// they wrote new state.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Default)]
pub struct InitOutcome {
    pub layout_created: bool,
    pub config_created: bool,
    pub constraints_config_created: bool,
}

impl InitOutcome {
    pub fn changed(&self) -> bool {
        self.layout_created || self.config_created || self.constraints_config_created
    }
}

const STORE_SCHEMA_VERSION: u32 = 2;

impl GraftStore {
    pub fn open(workspace: impl AsRef<Path>) -> Self {
        Self {
            paths: GraftPaths::new(workspace),
        }
    }

    pub fn paths(&self) -> &GraftPaths {
        &self.paths
    }

    pub fn init(&self) -> Result<InitOutcome> {
        let layout_existed = self.paths.root().exists();
        self.init_storage()?;
        if !self.paths.graft_config().exists() {
            fs::write(
                self.paths.graft_config(),
                "# Graft-local runtime config. User-facing project config remains in ../graft.toml for v1 compatibility.\n",
            )?;
        }
        let config_path = self.paths.config();
        let config_existed = config_path.exists();
        if !config_existed {
            fs::write(&config_path, DEFAULT_CONFIG)?;
        }
        let legacy_properties_config_path = self.paths.legacy_properties_config();
        let constraints_roto_path = self.paths.constraints_roto_config();
        let constraints_config_existed =
            legacy_properties_config_path.exists() || constraints_roto_path.exists();
        if !constraints_config_existed {
            write_default_constraints_roto_config(&constraints_roto_path)?;
        }
        Ok(InitOutcome {
            layout_created: !layout_existed,
            config_created: !config_existed,
            constraints_config_created: !constraints_config_existed,
        })
    }

    /// Returns true when this workspace has been initialized (graft.toml is
    /// present). Callers that require an initialized workspace should fail
    /// loud instead of silently falling back to default config.
    pub fn is_initialized(&self) -> bool {
        self.paths.config().exists()
    }

    pub fn init_storage(&self) -> Result<()> {
        self.migrate_legacy_local_dir()?;
        fs::create_dir_all(self.paths.object_blobs())?;
        fs::create_dir_all(self.paths.object_trees())?;
        fs::create_dir_all(self.paths.object_actions())?;
        fs::create_dir_all(self.paths.object_applications())?;
        fs::create_dir_all(self.paths.object_changes())?;
        fs::create_dir_all(self.paths.object_constraints())?;
        fs::create_dir_all(self.paths.object_plans())?;
        fs::create_dir_all(self.paths.object_patches())?;
        fs::create_dir_all(self.paths.object_evidence())?;
        fs::create_dir_all(self.paths.object_candidate_evidence_index())?;
        fs::create_dir_all(self.paths.object_patch_evidence_index())?;
        fs::create_dir_all(self.paths.cache_candidates())?;
        fs::create_dir_all(self.paths.cache_evidence())?;
        fs::create_dir_all(self.paths.derived_worktrees())?;
        fs::create_dir_all(self.paths.cache_trials())?;
        fs::create_dir_all(self.paths.cache_relations())?;
        fs::create_dir_all(self.paths.cache_worktrees())?;
        fs::create_dir_all(self.paths.cache_tmp())?;
        fs::create_dir_all(self.paths.registry_patches())?;
        fs::create_dir_all(self.paths.registry_evidence())?;
        fs::create_dir_all(self.paths.registry_relations())?;
        fs::create_dir_all(self.paths.registry_promotions())?;
        self.ensure_store_schema_version()?;
        fs::create_dir_all(self.paths.refs().join("drafts"))?;
        fs::create_dir_all(self.paths.refs().join("registry"))?;
        fs::create_dir_all(self.paths.materialized_refs())?;
        self.init_index()
    }
}

const DEFAULT_CONFIG: &str = r#"schema = 1

[admission.required]

[promotion.required]

[sync]
enabled = true
"#;

const DEFAULT_PROPERTIES_ROTO_CONFIG: &str = r#"// Graft v2 constraint source.
// Add top-level constraint functions with this shape:
//
// fn constraint_name(app: Application) -> Constraint {
//     top()
// }
"#;

fn write_default_constraints_roto_config(path: &Path) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(path, DEFAULT_PROPERTIES_ROTO_CONFIG)?;
    Ok(())
}

fn promoted_evidence_subject(subject: &str, candidate: &str, patch: &str) -> Option<String> {
    if subject == candidate {
        return Some(patch.to_string());
    }
    subject
        .strip_prefix(candidate)
        .and_then(|suffix| suffix.strip_prefix('@'))
        .map(|scope| format!("{patch}@{scope}"))
}

fn write_json(path: &Path, value: &impl serde::Serialize) -> Result<()> {
    let mut bytes = serde_json::to_vec_pretty(value)?;
    bytes.push(b'\n');
    atomic_write_file(path, &bytes)
}

fn atomic_write_file(path: &Path, bytes: &[u8]) -> Result<()> {
    let parent = store_write_parent(path)?;
    fs::create_dir_all(parent)?;
    let tmp_path = unique_store_write_sibling(parent, path)?;
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&tmp_path)
        .map_err(|error| StoreError::AtomicWrite {
            path: path.to_path_buf(),
            message: format!("create temp {}: {error}", tmp_path.display()),
        })?;

    if let Err(error) = file.write_all(bytes).and_then(|_| file.sync_all()) {
        let mut message = format!("write temp {}: {error}", tmp_path.display());
        if let Err(cleanup_error) = remove_path_if_exists(&tmp_path) {
            message.push_str(&format!(
                "; failed to clean temp {}: {cleanup_error}",
                tmp_path.display()
            ));
        }
        return Err(StoreError::AtomicWrite {
            path: path.to_path_buf(),
            message,
        });
    }
    drop(file);

    fs::rename(&tmp_path, path).map_err(|error| {
        let mut message = format!("rename temp {} into place: {error}", tmp_path.display());
        if let Err(cleanup_error) = remove_path_if_exists(&tmp_path) {
            message.push_str(&format!(
                "; failed to clean temp {}: {cleanup_error}",
                tmp_path.display()
            ));
        }
        StoreError::AtomicWrite {
            path: path.to_path_buf(),
            message,
        }
    })
}

fn store_write_parent(path: &Path) -> Result<&Path> {
    if path.file_name().is_none() {
        return Err(StoreError::InvalidStoreWritePath(path.to_path_buf()));
    }
    path.parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .ok_or_else(|| StoreError::InvalidStoreWritePath(path.to_path_buf()))
}

fn unique_store_write_sibling(parent: &Path, path: &Path) -> Result<PathBuf> {
    let Some(name) = path.file_name().and_then(|value| value.to_str()) else {
        return Err(StoreError::InvalidStoreWritePath(path.to_path_buf()));
    };
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|error| StoreError::AtomicWrite {
            path: path.to_path_buf(),
            message: format!("system clock before unix epoch: {error}"),
        })?
        .as_nanos();
    Ok(parent.join(format!(
        ".{name}.graft-json-{}-{nanos}.tmp",
        std::process::id()
    )))
}

fn read_evidence_index(dir: &Path, subject: &str) -> Result<Vec<String>> {
    read_evidence_refs_record(dir, subject).map(|refs| refs.evidence)
}

fn read_evidence_refs_record(dir: &Path, subject: &str) -> Result<EvidenceRefsRecord> {
    let path = dir.join(format!("{subject}.json"));
    let value: serde_json::Value = match read_json(&path) {
        Ok(value) => value,
        Err(StoreError::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(EvidenceRefsRecord {
                owner: subject.to_string(),
                evidence: Vec::new(),
                updated_at: None,
            });
        }
        Err(error) => return Err(error),
    };
    if value.is_object() {
        let refs: EvidenceRefsRecord = serde_json::from_value(value)?;
        if refs.owner != subject {
            return Err(StoreError::InvalidEvidenceIndex {
                path,
                message: format!("owner `{}` does not match subject `{subject}`", refs.owner),
            });
        }
        return Ok(refs);
    }
    Err(StoreError::InvalidEvidenceIndex {
        path,
        message: "expected evidence refs object with owner and evidence fields".to_string(),
    })
}

fn append_unique_index(dir: &Path, subject: &str, evidence: &str) -> Result<()> {
    fs::create_dir_all(dir)?;
    let path = dir.join(format!("{subject}.json"));
    let mut ids = read_evidence_index(dir, subject)?;
    if !ids.iter().any(|id| id == evidence) {
        ids.push(evidence.to_string());
    }
    let refs = EvidenceRefsRecord {
        owner: subject.to_string(),
        evidence: ids,
        updated_at: Some(time::OffsetDateTime::now_utc().to_string()),
    };
    write_json(&path, &refs)
}

fn read_json<T: serde::de::DeserializeOwned>(path: &Path) -> Result<T> {
    let bytes = fs::read(path)?;
    Ok(serde_json::from_slice(&bytes)?)
}

fn read_current_json<T: serde::de::DeserializeOwned>(path: &Path) -> Result<T> {
    let bytes = fs::read(path)?;
    let value: serde_json::Value = serde_json::from_slice(&bytes)?;
    reject_legacy_schema(path, &value)?;
    Ok(serde_json::from_value(value)?)
}

fn reject_legacy_schema(path: &Path, value: &serde_json::Value) -> Result<()> {
    let Some(object) = value.as_object() else {
        return Ok(());
    };
    let legacy_fields = ["expected", "properties", "admitted_at"]
        .into_iter()
        .filter(|field| object.contains_key(*field))
        .collect::<Vec<_>>();
    if legacy_fields.is_empty() {
        return Ok(());
    }
    Err(StoreError::UnsupportedStoreSchema {
        path: path.to_path_buf(),
        message: format!("legacy fields present: {}", legacy_fields.join(", ")),
    })
}

fn constraint_plans(constraint: &Constraint) -> Vec<PlanId> {
    let mut plans = Vec::new();
    collect_constraint_plans(constraint, &mut plans);
    plans
}

fn collect_constraint_plans(constraint: &Constraint, plans: &mut Vec<PlanId>) {
    match constraint {
        Constraint::Top | Constraint::Bottom => {}
        Constraint::Primitive { plan } => plans.push(plan.clone()),
        Constraint::Both { left, right } | Constraint::Either { left, right } => {
            collect_constraint_plans(left, plans);
            collect_constraint_plans(right, plans);
        }
    }
}

fn read_json_records<T: serde::de::DeserializeOwned>(path: &Path) -> Result<Vec<T>> {
    if !path.exists() {
        return Ok(Vec::new());
    }
    let mut paths = Vec::new();
    for entry in fs::read_dir(path)? {
        let entry = entry?;
        if entry.path().extension().and_then(|value| value.to_str()) == Some("json") {
            paths.push(entry.path());
        }
    }
    paths.sort();
    paths.into_iter().map(|path| read_json(&path)).collect()
}

struct NamedJsonRecord<T> {
    id: String,
    path: PathBuf,
    value: T,
}

fn read_named_json_records<T: serde::de::DeserializeOwned>(
    path: &Path,
) -> Result<Vec<NamedJsonRecord<T>>> {
    if !path.exists() {
        return Ok(Vec::new());
    }
    let paths = json_paths(path)?;
    paths
        .into_iter()
        .map(|path| {
            let id = json_file_stem(&path)?.ok_or_else(|| StoreError::InvalidStoreObjectPath {
                path: path.clone(),
                message: "expected a .json store object".to_string(),
            })?;
            let value = read_json(&path)?;
            Ok(NamedJsonRecord { id, path, value })
        })
        .collect()
}

fn read_relation_records(path: &Path, subject: &str) -> Result<Vec<PatchRelation>> {
    if !path.exists() {
        return Ok(Vec::new());
    }
    let mut records = Vec::new();
    let paths = json_paths(path)?;
    for path in paths {
        let record: PatchRelation = read_json(&path)?;
        if record.subject == subject {
            records.push(record);
        }
    }
    Ok(records)
}

fn json_file_stems(path: &Path) -> Result<Vec<String>> {
    json_paths(path)?
        .into_iter()
        .map(|path| {
            json_file_stem(&path)?.ok_or_else(|| StoreError::InvalidStoreObjectPath {
                path,
                message: "expected a .json store object".to_string(),
            })
        })
        .collect()
}

fn json_file_stem(path: &Path) -> Result<Option<String>> {
    if path.extension().and_then(|value| value.to_str()) != Some("json") {
        return Ok(None);
    }
    let Some(stem) = path.file_stem() else {
        return Err(StoreError::InvalidStoreObjectPath {
            path: path.to_path_buf(),
            message: "json store object has no file stem".to_string(),
        });
    };
    let stem = stem
        .to_str()
        .ok_or_else(|| StoreError::InvalidStoreObjectPath {
            path: path.to_path_buf(),
            message: "json store object name must be valid UTF-8".to_string(),
        })?;
    Ok(Some(stem.to_string()))
}

fn store_object_file_name(path: &Path, role: &str) -> Result<String> {
    let Some(name) = path.file_name() else {
        return Err(StoreError::InvalidStoreObjectPath {
            path: path.to_path_buf(),
            message: format!("{role} store object has no file name"),
        });
    };
    let name = name
        .to_str()
        .ok_or_else(|| StoreError::InvalidStoreObjectPath {
            path: path.to_path_buf(),
            message: format!("{role} store object name must be valid UTF-8"),
        })?;
    Ok(name.to_string())
}

fn json_paths(path: &Path) -> Result<Vec<PathBuf>> {
    let mut paths = Vec::new();
    for entry in fs::read_dir(path)? {
        let entry = entry?;
        if entry.path().extension().and_then(|value| value.to_str()) == Some("json") {
            paths.push(entry.path());
        }
    }
    paths.sort();
    Ok(paths)
}

fn collect_tree_entries(
    root: &Path,
    path: &Path,
    blob_dir: &Path,
    entries: &mut Vec<TreeEntry>,
) -> Result<()> {
    let mut children = Vec::new();
    for entry in fs::read_dir(path)? {
        children.push(entry?);
    }
    children.sort_by_key(|entry| entry.path());

    for entry in children {
        let path = entry.path();
        let file_name = entry.file_name();
        let file_type = entry.file_type()?;
        if file_type.is_dir() {
            let file_name = snapshot_entry_name(&path, &file_name)?;
            if file_name == ".git" {
                return Err(StoreError::InvalidSnapshotPath {
                    path,
                    message:
                        ".git directories are external VCS state and cannot be captured by Graft"
                            .to_string(),
                });
            }
            if should_skip_dir(file_name) {
                continue;
            }
            collect_tree_entries(root, &path, blob_dir, entries)?;
        } else if file_type.is_file() {
            let relative = normalize_relative_path(root, &path)?;
            let bytes = fs::read(&path)?;
            let hash = blake3::hash(&bytes).to_hex().to_string();
            let blob_path = blob_dir.join(&hash);
            if !blob_path.exists() {
                fs::write(blob_path, &bytes)?;
            }
            entries.push(TreeEntry {
                path: relative,
                hash,
                size: bytes.len() as u64,
            });
        }
    }
    Ok(())
}

fn snapshot_entry_name<'a>(path: &Path, file_name: &'a OsStr) -> Result<&'a str> {
    file_name
        .to_str()
        .ok_or_else(|| StoreError::InvalidSnapshotPath {
            path: path.to_path_buf(),
            message: "snapshot entry names must be valid UTF-8".to_string(),
        })
}

fn should_skip_dir(name: &str) -> bool {
    matches!(name, ".graft" | ".worktrees" | "worktrees")
}

fn should_skip_snapshot_path(path: &str) -> bool {
    path.split('/').any(should_skip_dir)
}

fn materialize_destination_parent(destination: &Path) -> Result<&Path> {
    if destination.file_name().is_none() {
        return Err(StoreError::InvalidMaterializeDestination(
            destination.to_path_buf(),
        ));
    }
    destination
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .ok_or_else(|| StoreError::InvalidMaterializeDestination(destination.to_path_buf()))
}

fn unique_materialize_sibling(parent: &Path, destination: &Path, role: &str) -> Result<PathBuf> {
    let Some(name) = destination.file_name().and_then(|value| value.to_str()) else {
        return Err(StoreError::InvalidMaterializeDestination(
            destination.to_path_buf(),
        ));
    };
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|error| StoreError::MaterializePublish {
            path: destination.to_path_buf(),
            message: format!("system clock before unix epoch: {error}"),
        })?
        .as_nanos();
    Ok(parent.join(format!(
        ".{name}.graft-{role}-{}-{nanos}",
        std::process::id()
    )))
}

fn publish_materialized_tree(stage: &Path, destination: &Path, backup: &Path) -> Result<()> {
    let had_destination = destination.exists();
    if had_destination {
        fs::rename(destination, backup).map_err(|error| StoreError::MaterializePublish {
            path: destination.to_path_buf(),
            message: format!("move previous destination to {}: {error}", backup.display()),
        })?;
    }

    match fs::rename(stage, destination) {
        Ok(()) => {
            if had_destination {
                remove_path_if_exists(backup).map_err(|error| StoreError::MaterializePublish {
                    path: destination.to_path_buf(),
                    message: format!("remove previous destination {}: {error}", backup.display()),
                })?;
            }
            Ok(())
        }
        Err(error) => {
            let mut message = format!("rename {} into place: {error}", stage.display());
            if had_destination {
                match fs::rename(backup, destination) {
                    Ok(()) => {}
                    Err(restore_error) => {
                        message.push_str(&format!(
                            "; failed to restore previous destination from {}: {restore_error}",
                            backup.display()
                        ));
                    }
                }
            }
            if let Err(cleanup_error) = remove_path_if_exists(stage) {
                message.push_str(&format!(
                    "; failed to clean staging directory {}: {cleanup_error}",
                    stage.display()
                ));
            }
            Err(StoreError::MaterializePublish {
                path: destination.to_path_buf(),
                message,
            })
        }
    }
}

fn remove_path_if_exists(path: &Path) -> Result<()> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(StoreError::Io(error)),
    };
    if metadata.is_dir() {
        fs::remove_dir_all(path)?;
    } else {
        fs::remove_file(path)?;
    }
    Ok(())
}

fn normalize_virtual_path(path: &str) -> Result<String> {
    Ok(validated_virtual_path_parts(path)?.join("/"))
}

fn materialized_path(root: &Path, relative: &str) -> Result<PathBuf> {
    let mut path = root.to_path_buf();
    for component in validated_virtual_path_parts(relative)? {
        path.push(component);
    }
    Ok(path)
}

fn validated_virtual_path_parts(path: &str) -> Result<Vec<&str>> {
    if path.is_empty()
        || path.contains('\n')
        || path.contains('\t')
        || path.contains('\\')
        || Path::new(path).is_absolute()
    {
        return Err(StoreError::InvalidPath(path.to_string()));
    }
    let mut parts = Vec::new();
    for component in path.split('/') {
        if component.is_empty() || component == "." || component == ".." {
            return Err(StoreError::InvalidPath(path.to_string()));
        }
        parts.push(component);
    }
    Ok(parts)
}

fn normalize_relative_path(root: &Path, path: &Path) -> Result<String> {
    let relative = path
        .strip_prefix(root)
        .map_err(|_| StoreError::InvalidSnapshotPath {
            path: path.to_path_buf(),
            message: "snapshot path is not under the snapshot root".to_string(),
        })?;
    let mut parts = Vec::new();
    for component in relative.components() {
        match component {
            Component::Normal(value) => {
                let value = value
                    .to_str()
                    .ok_or_else(|| StoreError::InvalidSnapshotPath {
                        path: path.to_path_buf(),
                        message: "snapshot path components must be valid UTF-8".to_string(),
                    })?;
                parts.push(value.to_string());
            }
            Component::CurDir
            | Component::ParentDir
            | Component::RootDir
            | Component::Prefix(_) => {
                return Err(StoreError::InvalidSnapshotPath {
                    path: path.to_path_buf(),
                    message: "snapshot path must be relative normal components".to_string(),
                });
            }
        }
    }
    if parts.is_empty() {
        return Err(StoreError::InvalidSnapshotPath {
            path: path.to_path_buf(),
            message: "snapshot path must not be empty".to_string(),
        });
    }
    Ok(parts.join("/"))
}

#[cfg(test)]
mod tests;
