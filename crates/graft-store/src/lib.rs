use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsStr;
use std::fs;
use std::io::Write;
use std::path::{Component, Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use graft_core::{
    Action, ActionId, ApplicationId, ApplicationRecord, ApplicationRef, CandidateId, Change,
    Constraint, EvidenceRecord, GraftCandidate, MaterializedApplication, PatchId, PatchRecord,
    PatchRelation, PromotionRecord, PropertyDef, PropertyId, PropertyRef, PropertySpec, StateId,
    TreeEntry, TreeSnapshot, action_id, application_id, evidence_id,
    validate_application_integrity,
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

/// Normalize a workspace-related path for registry keys and daemon engine keys.
///
/// Existing paths are fully canonicalized. For paths that do not exist yet,
/// the nearest existing parent is canonicalized and the missing suffix is
/// re-attached without lossy string conversion.
pub fn normalize_workspace_path(path: &Path) -> PathBuf {
    if let Ok(canonical) = path.canonicalize() {
        return canonical;
    }

    let mut missing = Vec::new();
    let mut cursor = path;
    while let Some(parent) = cursor.parent() {
        if let Some(name) = cursor.file_name() {
            missing.push(name.to_os_string());
        }
        if let Ok(mut canonical_parent) = parent.canonicalize() {
            for component in missing.iter().rev() {
                canonical_parent.push(component);
            }
            return canonical_parent;
        }
        cursor = parent;
    }

    path.to_path_buf()
}

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
pub struct GraftPaths {
    workspace: PathBuf,
    root: PathBuf,
}

impl GraftPaths {
    pub fn new(workspace: impl AsRef<Path>) -> Self {
        let workspace = workspace.as_ref().to_path_buf();
        let root = workspace.join(".graft");
        Self { workspace, root }
    }

    pub fn workspace(&self) -> &Path {
        &self.workspace
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn config(&self) -> PathBuf {
        self.workspace.join("graft.toml")
    }

    pub fn properties_config(&self) -> PathBuf {
        self.workspace.join("properties")
    }

    pub fn properties_roto_config(&self) -> PathBuf {
        self.workspace.join("properties.roto")
    }

    pub fn properties_lock(&self) -> PathBuf {
        self.workspace.join("graft.lock")
    }

    pub fn graft_config(&self) -> PathBuf {
        self.root.join("config.toml")
    }

    pub fn cache_candidates(&self) -> PathBuf {
        self.root.join("store").join("private").join("candidate")
    }

    pub fn cache_evidence(&self) -> PathBuf {
        self.root.join("store").join("derived").join("evidence")
    }

    pub fn cache_trials(&self) -> PathBuf {
        self.root.join("run").join("trials")
    }

    pub fn cache_relations(&self) -> PathBuf {
        self.root.join("store").join("private").join("relation")
    }

    pub fn cache_worktrees(&self) -> PathBuf {
        self.root.join("run").join("worktrees")
    }

    pub fn derived_worktrees(&self) -> PathBuf {
        self.root.join("store").join("derived").join("worktrees")
    }

    pub fn workspace_worktrees(&self) -> PathBuf {
        self.workspace.join(".worktrees")
    }

    pub fn cache_tmp(&self) -> PathBuf {
        self.root.join("run").join("tmp")
    }

    pub fn store_schema_version(&self) -> PathBuf {
        self.root.join("store").join("schema_version")
    }

    pub fn object_blobs(&self) -> PathBuf {
        self.root.join("store").join("public").join("blob")
    }

    pub fn object_trees(&self) -> PathBuf {
        self.root.join("store").join("public").join("tree")
    }

    pub fn object_changes(&self) -> PathBuf {
        self.root.join("store").join("public").join("change")
    }

    pub fn object_actions(&self) -> PathBuf {
        self.root.join("store").join("public").join("action")
    }

    pub fn object_applications(&self) -> PathBuf {
        self.root.join("store").join("public").join("application")
    }

    pub fn object_properties(&self) -> PathBuf {
        self.root.join("store").join("public").join("property")
    }

    pub fn object_patches(&self) -> PathBuf {
        self.root.join("store").join("public").join("patch")
    }

    pub fn object_evidence(&self) -> PathBuf {
        self.root.join("store").join("derived").join("evidence")
    }

    pub fn object_candidate_evidence_index(&self) -> PathBuf {
        self.root
            .join("store")
            .join("private")
            .join("evidence_refs")
    }

    pub fn object_patch_evidence_index(&self) -> PathBuf {
        self.root.join("store").join("public").join("evidence_refs")
    }

    pub fn registry_patches(&self) -> PathBuf {
        self.root.join("store").join("public").join("patch")
    }

    pub fn registry_evidence(&self) -> PathBuf {
        self.root.join("store").join("derived").join("evidence")
    }

    pub fn registry_relations(&self) -> PathBuf {
        self.root.join("store").join("public").join("relation")
    }

    pub fn registry_promotions(&self) -> PathBuf {
        self.root.join("store").join("public").join("promotion")
    }

    /// Workspace-local mutable bookkeeping (aliases, sync pointers, indexes).
    ///
    /// Named `local/` to avoid confusion with patch-theory [`StateId`] ("State").
    pub const LOCAL_DIR: &str = "local";

    const LEGACY_LOCAL_DIR: &str = "state";

    pub fn local_root(&self) -> PathBuf {
        self.root.join(Self::LOCAL_DIR)
    }

    pub fn index(&self) -> PathBuf {
        self.local_root().join("index.sqlite")
    }

    pub fn refs(&self) -> PathBuf {
        self.local_root().join("aliases")
    }

    pub fn default_sync_remote(&self) -> PathBuf {
        self.local_root().join("remotes").join("default")
    }

    pub fn remote_last_synced(&self, remote_key: &str) -> PathBuf {
        self.local_root()
            .join("remotes")
            .join(remote_key)
            .join("last_synced")
    }

    pub fn materialized_refs(&self) -> PathBuf {
        self.refs().join("graft").join("patches")
    }
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
    pub properties_config_created: bool,
}

impl InitOutcome {
    pub fn changed(&self) -> bool {
        self.layout_created || self.config_created || self.properties_config_created
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
        let properties_config_path = self.paths.properties_config();
        let properties_roto_path = self.paths.properties_roto_config();
        let properties_config_existed =
            properties_config_path.exists() || properties_roto_path.exists();
        if !properties_config_existed {
            write_default_properties_roto_config(&properties_roto_path)?;
        }
        Ok(InitOutcome {
            layout_created: !layout_existed,
            config_created: !config_existed,
            properties_config_created: !properties_config_existed,
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
        fs::create_dir_all(self.paths.object_properties())?;
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

    pub fn capture_worktree_snapshot(&self, worktree: impl AsRef<Path>) -> Result<TreeSnapshot> {
        let worktree = worktree.as_ref();
        fs::create_dir_all(self.paths.object_blobs())?;
        let mut entries = Vec::new();
        collect_tree_entries(worktree, worktree, &self.paths.object_blobs(), &mut entries)?;
        Ok(TreeSnapshot::new(entries))
    }

    pub fn capture_target_snapshot(
        &self,
        base: &TreeSnapshot,
        captured: &TreeSnapshot,
    ) -> TreeSnapshot {
        let mut entries = BTreeMap::new();
        for entry in &base.entries {
            if should_skip_snapshot_path(&entry.path) {
                entries.insert(entry.path.clone(), entry.clone());
            }
        }
        for entry in &captured.entries {
            if !should_skip_snapshot_path(&entry.path) {
                entries.insert(entry.path.clone(), entry.clone());
            }
        }
        TreeSnapshot::new(entries.into_values().collect())
    }

    pub fn restore_worktree_paths(
        &self,
        snapshot: &TreeSnapshot,
        worktree: impl AsRef<Path>,
        paths: &[String],
    ) -> Result<()> {
        let worktree = worktree.as_ref();
        for path in paths {
            let destination = materialized_path(worktree, path)?;
            match snapshot.entries.iter().find(|entry| entry.path == *path) {
                Some(entry) => {
                    if let Some(parent) = destination.parent() {
                        fs::create_dir_all(parent)?;
                    }
                    remove_path_if_exists(&destination)?;
                    fs::write(&destination, self.read_blob(&entry.hash)?)?;
                }
                None => {
                    remove_path_if_exists(&destination)?;
                }
            }
        }
        Ok(())
    }

    pub fn write_blob(&self, bytes: &[u8]) -> Result<String> {
        fs::create_dir_all(self.paths.object_blobs())?;
        let hash = blake3::hash(bytes).to_hex().to_string();
        let path = self.paths.object_blobs().join(&hash);
        if !path.exists() {
            fs::write(path, bytes)?;
        }
        Ok(hash)
    }

    pub fn write_blob_object(&self, hash: &str, bytes: &[u8]) -> Result<PathBuf> {
        let actual = blake3::hash(bytes).to_hex().to_string();
        if actual != hash {
            return Err(StoreError::BlobHashMismatch {
                expected: hash.to_string(),
                actual,
            });
        }
        fs::create_dir_all(self.paths.object_blobs())?;
        let path = self.paths.object_blobs().join(hash);
        if !path.exists() {
            fs::write(&path, bytes)?;
        }
        Ok(path)
    }

    pub fn read_blob(&self, hash: &str) -> Result<Vec<u8>> {
        Ok(fs::read(self.paths.object_blobs().join(hash))?)
    }

    pub fn list_blob_objects(&self) -> Result<Vec<(String, Vec<u8>)>> {
        if !self.paths.object_blobs().exists() {
            return Ok(Vec::new());
        }
        let mut paths = Vec::new();
        for entry in fs::read_dir(self.paths.object_blobs())? {
            let entry = entry?;
            if entry.file_type()?.is_file() {
                paths.push(entry.path());
            }
        }
        paths.sort();
        let mut blobs = Vec::new();
        for path in paths {
            let expected = store_object_file_name(&path, "blob")?;
            let bytes = fs::read(&path)?;
            let actual = blake3::hash(&bytes).to_hex().to_string();
            if actual != expected {
                return Err(StoreError::StoreObjectIdMismatch {
                    path,
                    expected,
                    actual,
                });
            }
            blobs.push((expected, bytes));
        }
        Ok(blobs)
    }

    pub fn blob_lookup_by_path(&self, snapshot: &TreeSnapshot, path: &str) -> Result<TreeEntry> {
        let path = normalize_virtual_path(path)?;
        if let Some(entry) = snapshot.entries.iter().find(|entry| entry.path == path) {
            return Ok(entry.clone());
        }
        let prefix = format!("{path}/");
        if snapshot
            .entries
            .iter()
            .any(|entry| entry.path.starts_with(&prefix))
        {
            return Err(StoreError::VirtualPathIsDirectory(path));
        }
        Err(StoreError::VirtualPathNotFound(path))
    }

    pub fn virtual_read(&self, snapshot: &TreeSnapshot, path: &str) -> Result<VirtualFile> {
        let entry = self.blob_lookup_by_path(snapshot, path)?;
        let bytes = self.read_blob(&entry.hash)?;
        Ok(VirtualFile {
            path: entry.path,
            hash: entry.hash,
            size: entry.size,
            bytes,
        })
    }

    pub fn virtual_tree(&self, base: &VirtualBaseRef) -> Result<TreeSnapshot> {
        match base {
            VirtualBaseRef::Empty => {
                let snapshot = TreeSnapshot::new(Vec::new());
                self.write_tree_snapshot(&snapshot)?;
                Ok(snapshot)
            }
            VirtualBaseRef::Tree(id) => self.read_tree_snapshot(id),
            VirtualBaseRef::Candidate(id) => {
                let candidate = self.read_candidate(id.as_str())?;
                let resolved = self.resolve_application(&candidate.application)?;
                self.virtual_tree_for_state(&resolved.record.target_state)
            }
            VirtualBaseRef::Patch(id) => {
                let patch = self.read_patch(id.as_str())?;
                let resolved = self.resolve_application(&patch.application)?;
                self.virtual_tree_for_state(&resolved.record.target_state)
            }
        }
    }

    pub fn virtual_tree_for_state(&self, state: &StateId) -> Result<TreeSnapshot> {
        match state {
            StateId::GraftTree(id) | StateId::GitTree(id) => self.read_tree_snapshot(id),
            StateId::RepoTree(repo) => Err(StoreError::UnsupportedVirtualBase(repo.display_ref())),
        }
    }

    pub fn materialize_tree_snapshot(
        &self,
        snapshot: &TreeSnapshot,
        destination: impl AsRef<Path>,
    ) -> Result<()> {
        let destination = destination.as_ref();
        let parent = materialize_destination_parent(destination)?;
        fs::create_dir_all(parent)?;
        let stage = unique_materialize_sibling(parent, destination, "stage")?;
        let backup = unique_materialize_sibling(parent, destination, "backup")?;

        fs::create_dir(&stage)?;
        if let Err(error) = self.write_materialized_snapshot(snapshot, &stage) {
            remove_path_if_exists(&stage)?;
            return Err(error);
        }
        publish_materialized_tree(&stage, destination, &backup)
    }

    fn write_materialized_snapshot(
        &self,
        snapshot: &TreeSnapshot,
        destination: &Path,
    ) -> Result<()> {
        for entry in &snapshot.entries {
            let path = materialized_path(destination, &entry.path)?;
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent)?;
            }
            fs::write(path, self.read_blob(&entry.hash)?)?;
        }
        Ok(())
    }

    pub fn write_tree_snapshot(&self, snapshot: &TreeSnapshot) -> Result<(String, PathBuf)> {
        fs::create_dir_all(self.paths.object_trees())?;
        let id = snapshot.id().map_err(StoreError::Core)?;
        let path = self.paths.object_trees().join(format!("{id}.json"));
        write_json(&path, snapshot)?;
        Ok((id, path))
    }

    pub fn read_tree_snapshot(&self, id: &str) -> Result<TreeSnapshot> {
        read_json(&self.paths.object_trees().join(format!("{id}.json")))
    }

    pub fn list_tree_objects(&self) -> Result<Vec<(String, TreeSnapshot)>> {
        read_named_json_records::<TreeSnapshot>(&self.paths.object_trees())?
            .into_iter()
            .map(|record| {
                let actual = record.value.id().map_err(StoreError::Core)?;
                if actual != record.id {
                    return Err(StoreError::StoreObjectIdMismatch {
                        path: record.path,
                        expected: record.id,
                        actual,
                    });
                }
                Ok((record.id, record.value))
            })
            .collect()
    }

    pub fn write_action(&self, action: &Action) -> Result<(ActionId, PathBuf)> {
        fs::create_dir_all(self.paths.object_actions())?;
        let id = action_id(action).map_err(StoreError::Core)?;
        let path = self.paths.object_actions().join(format!("{id}.json"));
        write_json(&path, action)?;
        Ok((id, path))
    }

    pub fn read_action(&self, id: &str) -> Result<Action> {
        read_json(&self.paths.object_actions().join(format!("{id}.json")))
    }

    pub fn write_application(
        &self,
        record: &ApplicationRecord,
    ) -> Result<(ApplicationId, PathBuf)> {
        fs::create_dir_all(self.paths.object_applications())?;
        let id = record.id().map_err(StoreError::Core)?;
        let path = self.paths.object_applications().join(format!("{id}.json"));
        write_json(&path, record)?;
        Ok((id, path))
    }

    pub fn read_application(&self, id: &str) -> Result<ApplicationRecord> {
        read_json(&self.paths.object_applications().join(format!("{id}.json")))
    }

    pub fn write_change(&self, change: &Change) -> Result<(graft_core::ChangeId, PathBuf)> {
        fs::create_dir_all(self.paths.object_changes())?;
        let id = change.id().map_err(StoreError::Core)?;
        let path = self.paths.object_changes().join(format!("{id}.json"));
        write_json(&path, change)?;
        Ok((id, path))
    }

    pub fn read_change(&self, id: &str) -> Result<Change> {
        read_json(&self.paths.object_changes().join(format!("{id}.json")))
    }

    pub fn write_materialized_application(
        &self,
        materialized: &MaterializedApplication,
    ) -> Result<ApplicationRef> {
        self.write_action(&materialized.action)?;
        self.write_change(&materialized.change)?;
        let (application_id, _) = self.write_application(&materialized.record)?;
        Ok(ApplicationRef::Stored(application_id))
    }

    pub fn resolve_application(&self, application: &ApplicationRef) -> Result<ResolvedApplication> {
        let ApplicationRef::Stored(expected_application_id) = application;
        let application_path = self
            .paths
            .object_applications()
            .join(format!("{expected_application_id}.json"));
        let record = self.read_application(expected_application_id.as_str())?;
        let actual_application_id = application_id(&record)
            .map_err(StoreError::Core)?
            .to_string();
        if actual_application_id != expected_application_id.as_str() {
            return Err(StoreError::StoreObjectIdMismatch {
                path: application_path,
                expected: expected_application_id.to_string(),
                actual: actual_application_id,
            });
        }
        let change = self.read_change(record.change.as_str())?;
        let action = self.read_action(record.action.as_str())?;
        validate_application_integrity(&record, &action, &change)
            .map_err(graft_core::CoreError::from)?;
        Ok(ResolvedApplication {
            record,
            change,
            action,
        })
    }

    pub fn list_change_objects(&self) -> Result<Vec<(String, Change)>> {
        read_named_json_records::<Change>(&self.paths.object_changes())?
            .into_iter()
            .map(|record| {
                let actual = record.value.id().map_err(StoreError::Core)?.to_string();
                if actual != record.id {
                    return Err(StoreError::StoreObjectIdMismatch {
                        path: record.path,
                        expected: record.id,
                        actual,
                    });
                }
                Ok((record.id, record.value))
            })
            .collect()
    }

    pub fn list_action_objects(&self) -> Result<Vec<(String, Action)>> {
        read_named_json_records::<Action>(&self.paths.object_actions())?
            .into_iter()
            .map(|record| {
                let actual = action_id(&record.value)
                    .map_err(StoreError::Core)?
                    .to_string();
                if actual != record.id {
                    return Err(StoreError::StoreObjectIdMismatch {
                        path: record.path,
                        expected: record.id,
                        actual,
                    });
                }
                Ok((record.id, record.value))
            })
            .collect()
    }

    pub fn list_application_objects(&self) -> Result<Vec<(String, ApplicationRecord)>> {
        read_named_json_records::<ApplicationRecord>(&self.paths.object_applications())?
            .into_iter()
            .map(|record| {
                let actual = record.value.id().map_err(StoreError::Core)?.to_string();
                if actual != record.id {
                    return Err(StoreError::StoreObjectIdMismatch {
                        path: record.path,
                        expected: record.id,
                        actual,
                    });
                }
                self.resolve_application(&ApplicationRef::Stored(ApplicationId::new(
                    record.id.clone(),
                )))?;
                Ok((record.id, record.value))
            })
            .collect()
    }

    pub fn write_property_def(&self, def: &PropertyDef) -> Result<(PropertyId, PathBuf)> {
        fs::create_dir_all(self.paths.object_properties())?;
        let id = def.property_id().map_err(StoreError::Core)?;
        let path = self.paths.object_properties().join(format!("{id}.json"));
        write_json(&path, def)?;
        Ok((id, path))
    }

    pub fn read_property_def(&self, id: &str) -> Result<PropertyDef> {
        read_json(&self.paths.object_properties().join(format!("{id}.json")))
    }

    pub fn write_property_spec(&self, spec: &PropertySpec) -> Result<(PropertyId, PathBuf)> {
        fs::create_dir_all(self.paths.object_properties())?;
        let id = spec.property_id().map_err(StoreError::Core)?;
        let path = self.paths.object_properties().join(format!("{id}.json"));
        write_json(&path, spec)?;
        Ok((id, path))
    }

    pub fn read_property_spec(&self, id: &str) -> Result<PropertySpec> {
        read_json(&self.paths.object_properties().join(format!("{id}.json")))
    }

    pub fn write_candidate(&self, candidate: &GraftCandidate) -> Result<PathBuf> {
        fs::create_dir_all(self.paths.cache_candidates())?;
        let path = self
            .paths
            .cache_candidates()
            .join(format!("{}.json", candidate.id));
        write_json(&path, candidate)?;
        self.write_candidate_evidence_index(candidate.id.as_str(), &[])?;
        Ok(path)
    }

    pub fn remove_candidate(&self, id: &str) -> Result<()> {
        match fs::remove_file(self.paths.cache_candidates().join(format!("{id}.json"))) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(StoreError::Io(error)),
        }
    }

    pub fn read_candidate(&self, id: &str) -> Result<GraftCandidate> {
        read_current_json(&self.paths.cache_candidates().join(format!("{id}.json")))
    }

    pub fn list_candidates(&self) -> Result<Vec<GraftCandidate>> {
        read_json_records(&self.paths.cache_candidates())
    }

    pub fn write_cache_relation(&self, relation: &PatchRelation) -> Result<PathBuf> {
        fs::create_dir_all(self.paths.cache_relations())?;
        let path = self
            .paths
            .cache_relations()
            .join(format!("{}.json", relation.id));
        write_json(&path, relation)?;
        Ok(path)
    }

    pub fn cached_relations_for_subject(&self, subject: &str) -> Result<Vec<PatchRelation>> {
        read_relation_records(&self.paths.cache_relations(), subject)
    }

    pub fn write_evidence(&self, evidence: &EvidenceRecord) -> Result<PathBuf> {
        fs::create_dir_all(self.paths.object_evidence())?;
        let path = self
            .paths
            .object_evidence()
            .join(format!("{}.json", evidence.id));
        write_json(&path, evidence)?;
        self.index_evidence(evidence)?;
        Ok(path)
    }

    pub fn read_evidence(&self, id: &str) -> Result<EvidenceRecord> {
        read_json(&self.paths.object_evidence().join(format!("{id}.json")))
    }

    pub fn candidate_evidence_index(&self, candidate: &str) -> Result<Vec<String>> {
        read_evidence_index(&self.paths.object_candidate_evidence_index(), candidate)
    }

    pub fn patch_evidence_index(&self, patch: &str) -> Result<Vec<String>> {
        read_evidence_index(&self.paths.object_patch_evidence_index(), patch)
    }

    pub fn write_candidate_evidence_index(
        &self,
        candidate: &str,
        evidence: &[String],
    ) -> Result<()> {
        fs::create_dir_all(self.paths.object_candidate_evidence_index())?;
        let refs = EvidenceRefsRecord {
            owner: candidate.to_string(),
            evidence: evidence.to_vec(),
            updated_at: Some(time::OffsetDateTime::now_utc().to_string()),
        };
        write_json(
            &self
                .paths
                .object_candidate_evidence_index()
                .join(format!("{candidate}.json")),
            &refs,
        )
    }

    pub fn remove_candidate_evidence_index(&self, candidate: &str) -> Result<()> {
        match fs::remove_file(
            self.paths
                .object_candidate_evidence_index()
                .join(format!("{candidate}.json")),
        ) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(StoreError::Io(error)),
        }
    }

    pub fn append_candidate_evidence_index(&self, candidate: &str, evidence: &str) -> Result<()> {
        append_unique_index(
            &self.paths.object_candidate_evidence_index(),
            candidate,
            evidence,
        )
    }

    pub fn append_patch_evidence_index(&self, patch: &str, evidence: &str) -> Result<()> {
        append_unique_index(&self.paths.object_patch_evidence_index(), patch, evidence)
    }

    pub fn copy_candidate_evidence_index_to_patch(
        &self,
        candidate: &str,
        patch: &str,
    ) -> Result<Vec<String>> {
        let index = self.candidate_evidence_index(candidate)?;
        let mut copied = Vec::new();
        for old_evidence_id in index {
            let mut evidence = self.read_evidence(&old_evidence_id)?;
            if let Some(subject) = promoted_evidence_subject(&evidence.subject, candidate, patch) {
                evidence.subject = subject;
                evidence.id = graft_core::EvidenceId::new("evidence:pending");
                evidence.id = evidence_id(&evidence).map_err(StoreError::Core)?;
                self.write_evidence(&evidence)?;
                copied.push(evidence.id.to_string());
            } else {
                copied.push(old_evidence_id);
            }
        }
        fs::create_dir_all(self.paths.object_patch_evidence_index())?;
        let refs = EvidenceRefsRecord {
            owner: patch.to_string(),
            evidence: copied.clone(),
            updated_at: Some(time::OffsetDateTime::now_utc().to_string()),
        };
        write_json(
            &self
                .paths
                .object_patch_evidence_index()
                .join(format!("{patch}.json")),
            &refs,
        )?;
        Ok(copied)
    }

    pub fn evidence_records_for_ids(&self, ids: &[String]) -> Result<Vec<EvidenceRecord>> {
        ids.iter().map(|id| self.read_evidence(id)).collect()
    }

    pub fn candidate_evidence_records(&self, candidate: &str) -> Result<Vec<EvidenceRecord>> {
        let ids = self.candidate_evidence_index(candidate)?;
        self.evidence_records_for_ids(&ids)
    }

    pub fn patch_evidence_records(&self, patch: &str) -> Result<Vec<EvidenceRecord>> {
        let ids = self.patch_evidence_index(patch)?;
        self.evidence_records_for_ids(&ids)
    }

    pub fn write_cache_evidence(&self, evidence: &EvidenceRecord) -> Result<PathBuf> {
        self.write_evidence(evidence)
    }

    pub fn cached_evidence_for_subject(&self, subject: &str) -> Result<Vec<EvidenceRecord>> {
        self.candidate_evidence_records(subject)
    }

    pub fn registry_evidence_for_subject(&self, subject: &str) -> Result<Vec<EvidenceRecord>> {
        self.patch_evidence_records(subject)
    }

    pub fn list_registry_evidence(&self) -> Result<Vec<EvidenceRecord>> {
        let mut ids = BTreeSet::new();
        for refs in self.list_patch_evidence_refs()? {
            ids.extend(refs.evidence);
        }
        let ids = ids.into_iter().collect::<Vec<_>>();
        self.evidence_records_for_ids(&ids)
    }

    pub fn write_registry_evidence(&self, evidence: &EvidenceRecord) -> Result<PathBuf> {
        self.write_evidence(evidence)
    }

    pub fn list_patch_evidence_refs(&self) -> Result<Vec<EvidenceRefsRecord>> {
        let dir = self.paths.object_patch_evidence_index();
        if !dir.exists() {
            return Ok(Vec::new());
        }
        json_file_stems(&dir)?
            .into_iter()
            .map(|owner| read_evidence_refs_record(&dir, &owner))
            .collect()
    }

    pub fn list_evidence_body_ids(&self) -> Result<Vec<String>> {
        json_file_stems(&self.paths.object_evidence())
    }

    pub fn list_candidate_evidence_ref_owners(&self) -> Result<Vec<String>> {
        json_file_stems(&self.paths.object_candidate_evidence_index())
    }

    pub fn list_patch_evidence_ref_owners(&self) -> Result<Vec<String>> {
        json_file_stems(&self.paths.object_patch_evidence_index())
    }

    pub fn write_patch_evidence_refs(&self, refs: &EvidenceRefsRecord) -> Result<()> {
        fs::create_dir_all(self.paths.object_patch_evidence_index())?;
        write_json(
            &self
                .paths
                .object_patch_evidence_index()
                .join(format!("{}.json", refs.owner)),
            refs,
        )
    }

    pub fn write_patch(&self, patch: &PatchRecord) -> Result<PathBuf> {
        fs::create_dir_all(self.paths.registry_patches())?;
        let path = self
            .paths
            .registry_patches()
            .join(format!("{}.json", patch.id));
        write_json(&path, patch)?;
        self.index_patch(patch)?;
        Ok(path)
    }

    pub fn write_patch_object(&self, patch: &PatchRecord) -> Result<PathBuf> {
        fs::create_dir_all(self.paths.object_patches())?;
        let path = self
            .paths
            .object_patches()
            .join(format!("{}.json", patch.id));
        write_json(&path, patch)?;
        Ok(path)
    }

    pub fn write_ref(&self, name: &str, value: &str) -> Result<PathBuf> {
        let path = self.paths.refs().join(name.trim_start_matches('/'));
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(&path, format!("{value}\n"))?;
        Ok(path)
    }

    pub fn read_patch(&self, id: &str) -> Result<PatchRecord> {
        read_current_json(&self.paths.registry_patches().join(format!("{id}.json")))
    }

    pub fn list_patches(&self) -> Result<Vec<PatchRecord>> {
        read_json_records(&self.paths.registry_patches())
    }

    pub fn write_relation(&self, relation: &PatchRelation) -> Result<PathBuf> {
        fs::create_dir_all(self.paths.registry_relations())?;
        let path = self
            .paths
            .registry_relations()
            .join(format!("{}.json", relation.id));
        write_json(&path, relation)?;
        Ok(path)
    }

    pub fn list_relations(&self) -> Result<Vec<PatchRelation>> {
        read_json_records(&self.paths.registry_relations())
    }

    pub fn write_promotion(&self, promotion: &PromotionRecord) -> Result<PathBuf> {
        fs::create_dir_all(self.paths.registry_promotions())?;
        let path = self
            .paths
            .registry_promotions()
            .join(format!("{}.json", promotion.id));
        write_json(&path, promotion)?;
        Ok(path)
    }

    pub fn list_promotions(&self) -> Result<Vec<PromotionRecord>> {
        read_json_records(&self.paths.registry_promotions())
    }

    pub fn search_patches_by_property(&self, property: &PropertyRef) -> Result<Vec<String>> {
        if !self.paths.index().exists() {
            return Ok(Vec::new());
        }
        let conn = Connection::open(self.paths.index())?;
        let property = serde_json::to_string(property)?;
        let mut statement = conn.prepare(
            "SELECT patch_id FROM patch_properties WHERE property = ?1 ORDER BY patch_id",
        )?;
        let rows = statement.query_map([property], |row| row.get::<_, String>(0))?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(StoreError::from)
    }

    fn ensure_store_schema_version(&self) -> Result<()> {
        let path = self.paths.store_schema_version();
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        if path.exists() {
            let value = fs::read_to_string(&path)?;
            let version =
                value
                    .trim()
                    .parse::<u32>()
                    .map_err(|_| StoreError::UnsupportedStoreSchema {
                        path: path.clone(),
                        message: format!(
                            "expected schema_version {STORE_SCHEMA_VERSION}, found {value:?}"
                        ),
                    })?;
            if version != STORE_SCHEMA_VERSION {
                return Err(StoreError::UnsupportedStoreSchema {
                    path,
                    message: format!(
                        "expected schema_version {STORE_SCHEMA_VERSION}, found {version}"
                    ),
                });
            }
            return Ok(());
        }
        fs::write(
            path,
            format!(
                "{STORE_SCHEMA_VERSION}
"
            ),
        )?;
        Ok(())
    }

    fn migrate_legacy_local_dir(&self) -> Result<()> {
        let legacy = self.paths.root().join(GraftPaths::LEGACY_LOCAL_DIR);
        let local = self.paths.local_root();
        if legacy.is_dir() && !local.exists() {
            fs::rename(&legacy, &local)?;
        }
        Ok(())
    }

    fn init_index(&self) -> Result<()> {
        if let Some(parent) = self.paths.index().parent() {
            fs::create_dir_all(parent)?;
        }
        let index_path = self.paths.index();
        if index_path.is_file() {
            let header = fs::read(&index_path).unwrap_or_default();
            if header.len() < 16 || &header[..16] != b"SQLite format 3\0" {
                fs::remove_file(&index_path)?;
            }
        }
        let conn = Connection::open(index_path)?;
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS patches (
                patch_id TEXT PRIMARY KEY,
                base_state TEXT NOT NULL,
                target_state TEXT NOT NULL,
                admitted_at TEXT NOT NULL
            );
            CREATE TABLE IF NOT EXISTS patch_properties (
                patch_id TEXT NOT NULL,
                property TEXT NOT NULL,
                PRIMARY KEY (patch_id, property)
            );
            CREATE TABLE IF NOT EXISTS evidence (
                evidence_id TEXT PRIMARY KEY,
                subject TEXT NOT NULL,
                property TEXT NOT NULL,
                result TEXT NOT NULL
            );",
        )?;
        Ok(())
    }

    fn index_patch(&self, patch: &PatchRecord) -> Result<()> {
        let resolved = self.resolve_application(&patch.application)?;
        let conn = Connection::open(self.paths.index())?;
        conn.execute(
            "INSERT OR REPLACE INTO patches (patch_id, base_state, target_state, admitted_at)
             VALUES (?1, ?2, ?3, ?4)",
            (
                patch.id.to_string(),
                serde_json::to_string(&resolved.record.base_state)?,
                serde_json::to_string(&resolved.record.target_state)?,
                patch.provenance.created_at.clone(),
            ),
        )?;
        for property in constraint_properties(&patch.constraint) {
            conn.execute(
                "INSERT OR REPLACE INTO patch_properties (patch_id, property) VALUES (?1, ?2)",
                (patch.id.to_string(), serde_json::to_string(&property)?),
            )?;
        }
        Ok(())
    }

    fn index_evidence(&self, evidence: &EvidenceRecord) -> Result<()> {
        let conn = Connection::open(self.paths.index())?;
        conn.execute(
            "INSERT OR REPLACE INTO evidence (evidence_id, subject, property, result)
             VALUES (?1, ?2, ?3, ?4)",
            (
                evidence.id.to_string(),
                evidence.subject.clone(),
                serde_json::to_string(&evidence.property)?,
                serde_json::to_string(&evidence.result)?,
            ),
        )?;
        Ok(())
    }
}

const DEFAULT_CONFIG: &str = r#"schema = 1

[admission.required_properties]

[promotion.required_properties]

[sync]
enabled = true
"#;

const DEFAULT_PROPERTIES_ROTO_CONFIG: &str = r#"// Graft v2 property source.
// Add top-level property functions with this shape:
//
// fn property_name(app: Application) -> Property {
//     property([unavailable("not configured yet")], "description", Severity.Info, [])
// }
"#;

fn write_default_properties_roto_config(path: &Path) -> Result<()> {
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

fn constraint_properties(constraint: &Constraint) -> Vec<PropertyRef> {
    let mut properties = Vec::new();
    collect_constraint_properties(constraint, &mut properties);
    properties
}

fn collect_constraint_properties(constraint: &Constraint, properties: &mut Vec<PropertyRef>) {
    match constraint {
        Constraint::Top | Constraint::Bottom => {}
        Constraint::Primitive { property } => properties.push(property.clone()),
        Constraint::Both { left, right } | Constraint::Either { left, right } => {
            collect_constraint_properties(left, properties);
            collect_constraint_properties(right, properties);
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
mod tests {
    use super::*;
    use graft_core::{
        AdmissionSummary, PatchId, PatchRelation, PatchRelationKind, PromotionId, PromotionRecord,
        PropertyId, Provenance, RelationId, StateId, materialize_application,
    };

    fn test_application_ref(
        store: &GraftStore,
        base_state: StateId,
        base: Option<&TreeSnapshot>,
        target_snapshot: &TreeSnapshot,
    ) -> ApplicationRef {
        let target_state = StateId::GraftTree(target_snapshot.id().unwrap());
        let materialized =
            materialize_application(base_state, base, target_state, target_snapshot).unwrap();
        store.write_materialized_application(&materialized).unwrap()
    }

    fn test_materialized_application() -> graft_core::MaterializedApplication {
        let target = TreeSnapshot::new(vec![TreeEntry {
            path: "hello.txt".to_string(),
            hash: "blob:hello".to_string(),
            size: 5,
        }]);
        materialize_application(
            StateId::GraftTree("tree:base".to_string()),
            None,
            StateId::GraftTree(target.id().unwrap()),
            &target,
        )
        .unwrap()
    }

    #[test]
    fn resolve_application_rejects_record_id_mismatch() {
        let dir = std::env::temp_dir().join(format!(
            "graft-store-application-id-mismatch-test-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        let store = GraftStore::open(&dir);
        store.init_storage().unwrap();
        let materialized = test_materialized_application();
        store.write_action(&materialized.action).unwrap();
        store.write_change(&materialized.change).unwrap();
        let actual_id = materialized.record.id().unwrap();
        let wrong_id = ApplicationId::new("application:wrong");
        fs::write(
            store
                .paths()
                .object_applications()
                .join(format!("{wrong_id}.json")),
            serde_json::to_vec(&materialized.record).unwrap(),
        )
        .unwrap();

        let error = store
            .resolve_application(&ApplicationRef::Stored(wrong_id))
            .unwrap_err()
            .to_string();

        assert!(error.contains("store object id mismatch"), "{error}");
        assert!(error.contains(actual_id.as_str()), "{error}");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn resolve_application_rejects_proof_action_mismatch() {
        let dir = std::env::temp_dir().join(format!(
            "graft-store-application-proof-mismatch-test-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        let store = GraftStore::open(&dir);
        store.init_storage().unwrap();
        let mut materialized = test_materialized_application();
        materialized.record.applicability_proof.action = ActionId::new("action:wrong");
        let application_id = materialized.record.id().unwrap();
        store.write_action(&materialized.action).unwrap();
        store.write_change(&materialized.change).unwrap();
        fs::write(
            store
                .paths()
                .object_applications()
                .join(format!("{application_id}.json")),
            serde_json::to_vec(&materialized.record).unwrap(),
        )
        .unwrap();

        let error = store
            .resolve_application(&ApplicationRef::Stored(application_id))
            .unwrap_err()
            .to_string();

        assert!(error.contains("[E_CHANGE_INTEGRITY]"), "{error}");
        assert!(error.contains("proof action"), "{error}");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn resolve_application_rejects_change_endpoint_mismatch() {
        let dir = std::env::temp_dir().join(format!(
            "graft-store-application-endpoint-mismatch-test-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        let store = GraftStore::open(&dir);
        store.init_storage().unwrap();
        let mut materialized = test_materialized_application();
        materialized.change.target_state = StateId::GraftTree("tree:wrong-target".to_string());
        materialized.record.change = materialized.change.id().unwrap();
        let application_id = materialized.record.id().unwrap();
        store.write_action(&materialized.action).unwrap();
        store.write_change(&materialized.change).unwrap();
        fs::write(
            store
                .paths()
                .object_applications()
                .join(format!("{application_id}.json")),
            serde_json::to_vec(&materialized.record).unwrap(),
        )
        .unwrap();

        let error = store
            .resolve_application(&ApplicationRef::Stored(application_id))
            .unwrap_err()
            .to_string();

        assert!(error.contains("[E_CHANGE_INTEGRITY]"), "{error}");
        assert!(error.contains("target_state"), "{error}");
        assert!(error.contains("change target_state"), "{error}");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn resolve_application_rejects_action_not_lowered_from_change() {
        let dir = std::env::temp_dir().join(format!(
            "graft-store-application-action-lowering-test-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        let store = GraftStore::open(&dir);
        store.init_storage().unwrap();
        let mut materialized = test_materialized_application();
        materialized.action = Action::Sequence { steps: Vec::new() };
        materialized.record.action = action_id(&materialized.action).unwrap();
        materialized.record.applicability_proof.action = materialized.record.action.clone();
        let application_id = materialized.record.id().unwrap();
        store.write_action(&materialized.action).unwrap();
        store.write_change(&materialized.change).unwrap();
        fs::write(
            store
                .paths()
                .object_applications()
                .join(format!("{application_id}.json")),
            serde_json::to_vec(&materialized.record).unwrap(),
        )
        .unwrap();

        let error = store
            .resolve_application(&ApplicationRef::Stored(application_id))
            .unwrap_err()
            .to_string();

        assert!(error.contains("[E_CHANGE_INTEGRITY]"), "{error}");
        assert!(error.contains("lowering from change ops"), "{error}");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn workspace_path_normalization_preserves_missing_suffix_under_canonical_parent() {
        let dir = std::env::temp_dir().join(format!(
            "graft-store-workspace-normalize-test-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        let parent = dir.join("existing");
        fs::create_dir_all(&parent).unwrap();

        let normalized = normalize_workspace_path(&parent.join("missing").join("workspace"));

        assert_eq!(
            normalized,
            parent
                .canonicalize()
                .unwrap()
                .join("missing")
                .join("workspace")
        );
        let _ = fs::remove_dir_all(&dir);
    }

    struct FailingSerialize;

    impl serde::Serialize for FailingSerialize {
        fn serialize<S>(&self, _serializer: S) -> std::result::Result<S::Ok, S::Error>
        where
            S: serde::Serializer,
        {
            Err(serde::ser::Error::custom(
                "intentional serialization failure",
            ))
        }
    }

    #[test]
    fn write_json_replaces_existing_file_without_temp_residue() {
        let dir = std::env::temp_dir().join(format!(
            "graft-store-atomic-json-test-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("record.json");
        fs::write(&path, "{\"old\":true}\n").unwrap();

        write_json(&path, &serde_json::json!({"new": true})).unwrap();

        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            "{\n  \"new\": true\n}\n"
        );
        assert!(fs::read_dir(&dir).unwrap().all(|entry| {
            !entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .contains("graft-json")
        }));

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn store_write_parent_requires_explicit_parent() {
        assert!(matches!(
            store_write_parent(Path::new("record.json")),
            Err(StoreError::InvalidStoreWritePath(_))
        ));
        assert_eq!(
            store_write_parent(Path::new("./record.json")).unwrap(),
            Path::new(".")
        );
        assert_eq!(
            store_write_parent(Path::new("objects/record.json")).unwrap(),
            Path::new("objects")
        );
    }

    #[test]
    fn write_json_preserves_existing_file_when_serialization_fails() {
        let dir = std::env::temp_dir().join(format!(
            "graft-store-atomic-json-serialize-fail-test-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("record.json");
        fs::write(&path, "{\"old\":true}\n").unwrap();

        let error = write_json(&path, &FailingSerialize)
            .unwrap_err()
            .to_string();

        assert!(
            error.contains("intentional serialization failure"),
            "{error}"
        );
        assert_eq!(fs::read_to_string(&path).unwrap(), "{\"old\":true}\n");
        assert!(fs::read_dir(&dir).unwrap().all(|entry| {
            !entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .contains("graft-json")
        }));

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn init_creates_sqlite_index() {
        let dir = std::env::temp_dir().join(format!("graft-store-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let store = GraftStore::open(&dir);
        store.init().unwrap();
        assert!(store.paths().index().exists());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn init_storage_does_not_create_project_config() {
        let dir = std::env::temp_dir().join(format!(
            "graft-store-storage-config-test-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);

        let store = GraftStore::open(&dir);
        store.init_storage().unwrap();

        assert!(store.paths().index().exists());
        assert!(store.paths().derived_worktrees().exists());
        assert_eq!(
            fs::read_to_string(store.paths().store_schema_version()).unwrap(),
            "2
"
        );
        assert!(!dir.join("graft.toml").exists());

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn init_creates_root_config() {
        let dir = std::env::temp_dir().join(format!(
            "graft-store-root-config-test-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();

        let store = GraftStore::open(&dir);
        store.init().unwrap();

        let config = fs::read_to_string(dir.join("graft.toml")).unwrap();
        assert!(config.contains("schema = 1"));
        assert!(config.contains("[sync]"));
        assert!(config.contains("enabled = true"));
        assert!(!config.contains("[create]"));
        assert!(!config.contains("default_base"));
        assert!(!config.contains("default_mode"));
        assert!(!dir.join("properties").exists());
        let properties_roto = fs::read_to_string(dir.join("properties.roto")).unwrap();
        assert!(properties_roto.contains("Graft v2 property source"));
        assert!(properties_roto.contains("fn property_name(app: Application) -> Property"));
        assert!(!dir.join("graft.lock").exists());

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn captures_and_stores_tree_snapshot() {
        let dir =
            std::env::temp_dir().join(format!("graft-store-snapshot-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("src")).unwrap();
        fs::write(dir.join("src").join("lib.rs"), "pub fn demo() {}\n").unwrap();

        let store = GraftStore::open(&dir);
        store.init_storage().unwrap();
        let snapshot = store.capture_worktree_snapshot(&dir).unwrap();
        assert_eq!(snapshot.entries.len(), 1);
        let blob = store.read_blob(&snapshot.entries[0].hash).unwrap();
        assert_eq!(blob, b"pub fn demo() {}\n");
        let (id, _) = store.write_tree_snapshot(&snapshot).unwrap();
        assert!(id.starts_with("tree:"));
        assert_eq!(store.read_tree_snapshot(&id).unwrap(), snapshot);

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn snapshot_relative_path_rejects_paths_outside_root() {
        let error =
            normalize_relative_path(Path::new("root"), Path::new("other/file.txt")).unwrap_err();

        assert!(matches!(
            error,
            StoreError::InvalidSnapshotPath { message, .. }
                if message.contains("not under the snapshot root")
        ));
    }

    #[test]
    fn snapshot_relative_path_rejects_parent_components() {
        let error = normalize_relative_path(Path::new("root"), Path::new("root/../escape.txt"))
            .unwrap_err();

        assert!(matches!(
            error,
            StoreError::InvalidSnapshotPath { message, .. }
                if message.contains("relative normal components")
        ));
    }

    #[cfg(unix)]
    #[test]
    fn snapshot_relative_path_rejects_non_utf8_components() {
        use std::os::unix::ffi::OsStrExt;

        let root = Path::new("root");
        let mut path = root.to_path_buf();
        path.push(OsStr::from_bytes(b"bad-\xFF"));

        let error = normalize_relative_path(root, &path).unwrap_err();

        assert!(matches!(
            error,
            StoreError::InvalidSnapshotPath { message, .. }
                if message.contains("valid UTF-8")
        ));
    }

    #[cfg(unix)]
    #[test]
    fn snapshot_entry_name_rejects_non_utf8_names_before_skip_rules() {
        use std::os::unix::ffi::OsStrExt;

        let name = OsStr::from_bytes(b".graft-\xFF");
        let error = snapshot_entry_name(Path::new("root/non-utf8-name"), name).unwrap_err();

        assert!(matches!(
            error,
            StoreError::InvalidSnapshotPath { message, .. }
                if message.contains("entry names must be valid UTF-8")
        ));
    }

    #[test]
    fn list_blob_objects_rejects_content_address_mismatch() {
        let dir = std::env::temp_dir().join(format!(
            "graft-store-blob-mismatch-test-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        let store = GraftStore::open(&dir);
        store.init_storage().unwrap();
        fs::write(store.paths().object_blobs().join("deadbeef"), b"demo\n").unwrap();

        let error = store.list_blob_objects().unwrap_err().to_string();

        assert!(error.contains("store object id mismatch"), "{error}");
        assert!(error.contains("expected deadbeef"), "{error}");

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn list_tree_objects_rejects_filename_that_disagrees_with_content_id() {
        let dir = std::env::temp_dir().join(format!(
            "graft-store-tree-mismatch-test-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        let store = GraftStore::open(&dir);
        store.init_storage().unwrap();
        let snapshot = TreeSnapshot::new(Vec::new());
        let actual = snapshot.id().unwrap();
        write_json(
            &store
                .paths()
                .object_trees()
                .join("tree:not-the-content.json"),
            &snapshot,
        )
        .unwrap();

        let error = store.list_tree_objects().unwrap_err().to_string();

        assert!(error.contains("store object id mismatch"), "{error}");
        assert!(error.contains("expected tree:not-the-content"), "{error}");
        assert!(error.contains(&actual), "{error}");

        let _ = fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn json_file_stem_rejects_non_utf8_object_names() {
        use std::ffi::OsString;
        use std::os::unix::ffi::OsStringExt;

        let path = PathBuf::from(OsString::from_vec(b"tree:\xFF.json".to_vec()));
        let error = json_file_stem(&path).unwrap_err().to_string();

        assert!(error.contains("invalid store object path"), "{error}");
        assert!(error.contains("valid UTF-8"), "{error}");
    }

    #[test]
    fn init_storage_rejects_unsupported_store_schema_marker() {
        let dir = std::env::temp_dir().join(format!(
            "graft-store-schema-marker-test-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        let store = GraftStore::open(&dir);
        fs::create_dir_all(store.paths().store_schema_version().parent().unwrap()).unwrap();
        fs::write(
            store.paths().store_schema_version(),
            "1
",
        )
        .unwrap();

        let err = store.init_storage().unwrap_err();

        assert!(matches!(err, StoreError::UnsupportedStoreSchema { .. }));
        assert!(err.to_string().contains("[E_UNSUPPORTED_STORE_SCHEMA]"));

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn capture_worktree_snapshot_tracks_workspace_meta_and_only_skips_graft_worktrees() {
        let dir = std::env::temp_dir().join(format!(
            "graft-store-snapshot-worktrees-test-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join(".worktrees").join("patch:demo")).unwrap();
        fs::create_dir_all(dir.join("worktrees").join("repo-a")).unwrap();
        fs::create_dir_all(dir.join("dist")).unwrap();
        fs::create_dir_all(dir.join("target")).unwrap();
        fs::create_dir_all(dir.join("properties")).unwrap();
        fs::write(dir.join("graft.toml"), "schema = 1\n").unwrap();
        fs::write(dir.join("graft.lock"), "version = 1\n").unwrap();
        fs::write(dir.join("properties.roto"), "fn prop() {}\n").unwrap();
        fs::write(dir.join("tracked.txt"), "tracked\n").unwrap();
        fs::write(
            dir.join(".worktrees")
                .join("patch:demo")
                .join("generated.txt"),
            "materialized\n",
        )
        .unwrap();
        fs::write(
            dir.join("worktrees").join("repo-a").join("tracked.txt"),
            "repo state\n",
        )
        .unwrap();
        fs::write(dir.join("dist").join("bundle.js"), "dist\n").unwrap();
        fs::write(dir.join("target").join("artifact"), "target\n").unwrap();
        fs::write(dir.join("properties").join("custom.roto"), "property\n").unwrap();

        let store = GraftStore::open(&dir);
        store.init_storage().unwrap();
        let snapshot = store.capture_worktree_snapshot(&dir).unwrap();

        assert_eq!(
            snapshot
                .entries
                .iter()
                .map(|entry| entry.path.as_str())
                .collect::<Vec<_>>(),
            vec![
                "dist/bundle.js",
                "graft.lock",
                "graft.toml",
                "properties.roto",
                "properties/custom.roto",
                "target/artifact",
                "tracked.txt"
            ]
        );

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn capture_worktree_snapshot_rejects_git_dir() {
        let dir = std::env::temp_dir().join(format!(
            "graft-store-snapshot-git-test-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join(".git")).unwrap();
        fs::write(dir.join("tracked.txt"), "tracked\n").unwrap();

        let store = GraftStore::open(&dir);
        store.init_storage().unwrap();
        let error = store
            .capture_worktree_snapshot(&dir)
            .unwrap_err()
            .to_string();

        assert!(error.contains("invalid snapshot path"), "{error}");
        assert!(error.contains(".git directories"), "{error}");

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn capture_target_snapshot_preserves_skipped_base_paths() {
        let base = TreeSnapshot::new(vec![
            TreeEntry {
                path: "src/lib.rs".to_string(),
                hash: "old".to_string(),
                size: 3,
            },
            TreeEntry {
                path: "worktrees/A/value.txt".to_string(),
                hash: "repo".to_string(),
                size: 4,
            },
            TreeEntry {
                path: "graft.toml".to_string(),
                hash: "config".to_string(),
                size: 6,
            },
        ]);
        let captured = TreeSnapshot::new(vec![TreeEntry {
            path: "src/lib.rs".to_string(),
            hash: "new".to_string(),
            size: 3,
        }]);
        let dir = std::env::temp_dir().join(format!(
            "graft-store-capture-target-test-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        let store = GraftStore::open(&dir);

        let target = store.capture_target_snapshot(&base, &captured);

        assert_eq!(
            target
                .entries
                .iter()
                .map(|entry| (entry.path.as_str(), entry.hash.as_str()))
                .collect::<Vec<_>>(),
            vec![("src/lib.rs", "new"), ("worktrees/A/value.txt", "repo")]
        );
    }

    #[test]
    fn restore_worktree_paths_applies_snapshot_entries_and_removals() {
        let dir = std::env::temp_dir().join(format!(
            "graft-store-restore-paths-test-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("src")).unwrap();
        fs::write(dir.join("src").join("lib.rs"), "dirty\n").unwrap();
        fs::write(dir.join("added.txt"), "added\n").unwrap();
        let store = GraftStore::open(&dir);
        store.init_storage().unwrap();
        let base_hash = store.write_blob(b"base\n").unwrap();
        let snapshot = TreeSnapshot::new(vec![TreeEntry {
            path: "src/lib.rs".to_string(),
            hash: base_hash,
            size: 5,
        }]);

        store
            .restore_worktree_paths(
                &snapshot,
                &dir,
                &["src/lib.rs".to_string(), "added.txt".to_string()],
            )
            .unwrap();

        assert_eq!(
            fs::read_to_string(dir.join("src").join("lib.rs")).unwrap(),
            "base\n"
        );
        assert!(!dir.join("added.txt").exists());

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn materializes_tree_snapshot_to_directory() {
        let dir = std::env::temp_dir().join(format!(
            "graft-store-materialize-test-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        let store = GraftStore::open(&dir);
        store.init_storage().unwrap();
        let hash = store.write_blob(b"demo\n").unwrap();
        let snapshot = TreeSnapshot::new(vec![TreeEntry {
            path: "src/lib.rs".to_string(),
            hash,
            size: 5,
        }]);
        let destination = dir.join("out");

        store
            .materialize_tree_snapshot(&snapshot, &destination)
            .unwrap();

        assert_eq!(
            fs::read_to_string(destination.join("src").join("lib.rs")).unwrap(),
            "demo\n"
        );

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn materialize_tree_snapshot_replaces_existing_destination_after_staging() {
        let dir = std::env::temp_dir().join(format!(
            "graft-store-materialize-replace-test-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        let store = GraftStore::open(&dir);
        store.init_storage().unwrap();
        let hash = store.write_blob(b"new\n").unwrap();
        let snapshot = TreeSnapshot::new(vec![TreeEntry {
            path: "new.txt".to_string(),
            hash,
            size: 4,
        }]);
        let destination = dir.join("out");
        fs::create_dir_all(&destination).unwrap();
        fs::write(destination.join("old.txt"), "old\n").unwrap();

        store
            .materialize_tree_snapshot(&snapshot, &destination)
            .unwrap();

        assert!(!destination.join("old.txt").exists());
        assert_eq!(
            fs::read_to_string(destination.join("new.txt")).unwrap(),
            "new\n"
        );
        assert!(fs::read_dir(&dir).unwrap().all(|entry| {
            !entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .contains("graft-backup")
        }));

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn materialize_destination_parent_requires_explicit_parent() {
        assert!(matches!(
            materialize_destination_parent(Path::new("out")),
            Err(StoreError::InvalidMaterializeDestination(_))
        ));
        assert_eq!(
            materialize_destination_parent(Path::new("./out")).unwrap(),
            Path::new(".")
        );
        assert_eq!(
            materialize_destination_parent(Path::new(".worktrees/out")).unwrap(),
            Path::new(".worktrees")
        );
    }

    #[test]
    fn materialize_tree_snapshot_preserves_destination_when_staging_fails() {
        let dir = std::env::temp_dir().join(format!(
            "graft-store-materialize-fail-preserve-test-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        let store = GraftStore::open(&dir);
        store.init_storage().unwrap();
        let hash = store.write_blob(b"new\n").unwrap();
        let snapshot = TreeSnapshot::new(vec![TreeEntry {
            path: "../escape".to_string(),
            hash,
            size: 4,
        }]);
        let destination = dir.join("out");
        fs::create_dir_all(&destination).unwrap();
        fs::write(destination.join("old.txt"), "old\n").unwrap();

        assert!(matches!(
            store.materialize_tree_snapshot(&snapshot, &destination),
            Err(StoreError::InvalidPath(_))
        ));
        assert_eq!(
            fs::read_to_string(destination.join("old.txt")).unwrap(),
            "old\n"
        );
        assert!(fs::read_dir(&dir).unwrap().all(|entry| {
            !entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .contains("graft-stage")
        }));

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn virtual_read_reads_blob_by_exact_snapshot_path() {
        let dir = std::env::temp_dir().join(format!(
            "graft-store-virtual-read-test-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        let store = GraftStore::open(&dir);
        store.init_storage().unwrap();
        let hash = store.write_blob(b"demo\n").unwrap();
        let snapshot = TreeSnapshot::new(vec![TreeEntry {
            path: "src/lib.rs".to_string(),
            hash: hash.clone(),
            size: 5,
        }]);

        let file = store.virtual_read(&snapshot, "src/lib.rs").unwrap();

        assert_eq!(file.path, "src/lib.rs");
        assert_eq!(file.hash, hash);
        assert_eq!(file.size, 5);
        assert_eq!(file.bytes, b"demo\n");

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn virtual_read_distinguishes_missing_files_and_directories() {
        let dir = std::env::temp_dir().join(format!(
            "graft-store-virtual-path-test-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        let store = GraftStore::open(&dir);
        store.init_storage().unwrap();
        let hash = store.write_blob(b"demo\n").unwrap();
        let snapshot = TreeSnapshot::new(vec![TreeEntry {
            path: "src/lib.rs".to_string(),
            hash,
            size: 5,
        }]);

        assert!(matches!(
            store.virtual_read(&snapshot, "src"),
            Err(StoreError::VirtualPathIsDirectory(path)) if path == "src"
        ));
        assert!(matches!(
            store.virtual_read(&snapshot, "missing.rs"),
            Err(StoreError::VirtualPathNotFound(path)) if path == "missing.rs"
        ));
        assert!(matches!(
            store.virtual_read(&snapshot, "../escape"),
            Err(StoreError::InvalidPath(path)) if path == "../escape"
        ));
        for path in [
            "",
            "/absolute",
            ".",
            "./src/lib.rs",
            "src//lib.rs",
            "src/",
            "src/../lib.rs",
            "src/./lib.rs",
            "src\\lib.rs",
            "line\nbreak",
            "tab\tpath",
        ] {
            assert!(
                matches!(
                    store.virtual_read(&snapshot, path),
                    Err(StoreError::InvalidPath(_))
                ),
                "path should be rejected: {path:?}"
            );
        }

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn virtual_tree_resolves_candidate_and_patch_targets() {
        let dir = std::env::temp_dir().join(format!(
            "graft-store-virtual-tree-test-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        let store = GraftStore::open(&dir);
        store.init().unwrap();
        let hash = store.write_blob(b"demo\n").unwrap();
        let snapshot = TreeSnapshot::new(vec![TreeEntry {
            path: "src/lib.rs".to_string(),
            hash,
            size: 5,
        }]);
        let (tree_id, _) = store.write_tree_snapshot(&snapshot).unwrap();

        let application = test_application_ref(
            &store,
            StateId::GraftTree("tree:base".to_string()),
            None,
            &snapshot,
        );

        let candidate = GraftCandidate {
            id: graft_core::CandidateId::new("candidate:demo"),
            application: application.clone(),
            constraint: Constraint::Top,
            provenance: Provenance {
                producer: "test".to_string(),
                message: None,
                created_at: "now".to_string(),
            },
        };
        store.write_candidate(&candidate).unwrap();

        let patch = PatchRecord {
            id: PatchId::new("patch:demo"),
            application,
            constraint: Constraint::Top,
            provenance: Provenance {
                producer: "test".to_string(),
                message: None,
                created_at: "now".to_string(),
            },
            admission: AdmissionSummary {
                constraint: Constraint::Top,
            },
        };
        store.write_patch(&patch).unwrap();

        assert_eq!(
            store
                .virtual_tree(&VirtualBaseRef::Candidate(candidate.id.clone()))
                .unwrap(),
            snapshot
        );
        assert_eq!(
            store
                .virtual_tree(&VirtualBaseRef::Patch(patch.id.clone()))
                .unwrap(),
            snapshot
        );
        assert_eq!(
            store.virtual_tree(&VirtualBaseRef::Tree(tree_id)).unwrap(),
            snapshot
        );

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn read_candidate_rejects_legacy_expected_field() {
        let dir = std::env::temp_dir().join(format!(
            "graft-store-legacy-candidate-schema-test-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        let store = GraftStore::open(&dir);
        store.init().unwrap();
        fs::create_dir_all(store.paths.cache_candidates()).unwrap();
        fs::write(
            store.paths.cache_candidates().join("candidate:legacy.json"),
            r#"{
              "id":"candidate:legacy",
              "application":{"kind":"stored","value":"application:demo"},
              "expected":[],
              "provenance":{"producer":"test","message":null,"created_at":"now"}
            }"#,
        )
        .unwrap();

        let err = store.read_candidate("candidate:legacy").unwrap_err();

        assert!(matches!(err, StoreError::UnsupportedStoreSchema { .. }));
        assert!(err.to_string().contains("[E_UNSUPPORTED_STORE_SCHEMA]"));

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn materialization_rejects_non_normal_snapshot_paths() {
        let dir = std::env::temp_dir().join(format!(
            "graft-store-materialize-path-test-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        let store = GraftStore::open(&dir);
        store.init_storage().unwrap();
        let hash = store.write_blob(b"demo\n").unwrap();
        for path in [
            "",
            "/absolute",
            ".",
            "./file.txt",
            "dir//file.txt",
            "dir/",
            "../escape",
            "dir/../escape",
            "dir/./file.txt",
            "dir\\file.txt",
            "line\nbreak",
            "tab\tpath",
        ] {
            let snapshot = TreeSnapshot::new(vec![TreeEntry {
                path: path.to_string(),
                hash: hash.clone(),
                size: 5,
            }]);

            assert!(
                matches!(
                    store.materialize_tree_snapshot(&snapshot, dir.join("out")),
                    Err(StoreError::InvalidPath(_))
                ),
                "path should be rejected: {path:?}"
            );
        }

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn registry_search_returns_only_indexed_patches() {
        let dir =
            std::env::temp_dir().join(format!("graft-store-search-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let store = GraftStore::open(&dir);
        store.init().unwrap();

        let property = PropertyRef::new(PropertyId::new("property:observation"), "ObservationOnly");
        let hash = store.write_blob(b"demo\n").unwrap();
        let snapshot = TreeSnapshot::new(vec![TreeEntry {
            path: "src/lib.rs".to_string(),
            hash,
            size: 5,
        }]);
        store.write_tree_snapshot(&snapshot).unwrap();
        let application = test_application_ref(
            &store,
            StateId::GitTree("base".to_string()),
            None,
            &snapshot,
        );
        let patch = PatchRecord {
            id: PatchId::new("patch:demo"),
            application,
            constraint: Constraint::primitive(property.clone()),
            provenance: Provenance {
                producer: "test".to_string(),
                message: None,
                created_at: "now".to_string(),
            },
            admission: AdmissionSummary {
                constraint: Constraint::primitive(property.clone()),
            },
        };
        store.write_patch(&patch).unwrap();

        assert_eq!(
            store.search_patches_by_property(&property).unwrap(),
            vec!["patch:demo".to_string()]
        );

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn copies_candidate_evidence_index_to_patch_with_patch_subjects() {
        let dir = std::env::temp_dir().join(format!(
            "graft-store-evidence-index-test-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        let store = GraftStore::open(&dir);
        store.init_storage().unwrap();

        let property = PropertyId::new("property:indexcopy");
        let evidence = EvidenceRecord::passed("candidate:demo", property, "test-verifier").unwrap();
        let old_evidence_id = evidence.id.to_string();
        store.write_evidence(&evidence).unwrap();
        store
            .append_candidate_evidence_index("candidate:demo", &old_evidence_id)
            .unwrap();

        let copied = store
            .copy_candidate_evidence_index_to_patch("candidate:demo", "patch:demo")
            .unwrap();

        assert_ne!(copied, vec![old_evidence_id]);
        assert_eq!(store.patch_evidence_index("patch:demo").unwrap(), copied);
        let patch_evidence = store.patch_evidence_records("patch:demo").unwrap();
        assert_eq!(patch_evidence[0].subject, "patch:demo");
        assert_eq!(
            patch_evidence[0].property,
            PropertyId::new("property:indexcopy")
        );
        assert_eq!(
            read_json_records::<EvidenceRecord>(&store.paths().object_evidence())
                .unwrap()
                .len(),
            2
        );

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn evidence_body_is_not_authoritative_without_owner_refs() {
        let dir = std::env::temp_dir().join(format!(
            "graft-store-evidence-ref-authority-test-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        let store = GraftStore::open(&dir);
        store.init_storage().unwrap();

        let candidate_evidence = EvidenceRecord::passed(
            "candidate:demo",
            PropertyId::new("property:candidate"),
            "test",
        )
        .unwrap();
        let patch_evidence =
            EvidenceRecord::passed("patch:demo", PropertyId::new("property:patch"), "test")
                .unwrap();
        let candidate_evidence_id = candidate_evidence.id.to_string();
        let patch_evidence_id = patch_evidence.id.to_string();
        store.write_evidence(&candidate_evidence).unwrap();
        store.write_evidence(&patch_evidence).unwrap();

        assert_eq!(
            store.cached_evidence_for_subject("candidate:demo").unwrap(),
            Vec::<EvidenceRecord>::new()
        );
        assert_eq!(
            store.registry_evidence_for_subject("patch:demo").unwrap(),
            Vec::<EvidenceRecord>::new()
        );
        assert_eq!(
            store.list_registry_evidence().unwrap(),
            Vec::<EvidenceRecord>::new()
        );

        store
            .append_candidate_evidence_index("candidate:demo", &candidate_evidence_id)
            .unwrap();
        store
            .append_patch_evidence_index("patch:demo", &patch_evidence_id)
            .unwrap();

        assert_eq!(
            store.cached_evidence_for_subject("candidate:demo").unwrap(),
            vec![candidate_evidence]
        );
        assert_eq!(
            store.registry_evidence_for_subject("patch:demo").unwrap(),
            vec![patch_evidence.clone()]
        );
        assert_eq!(
            store.list_registry_evidence().unwrap(),
            vec![patch_evidence]
        );

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn evidence_index_rejects_legacy_array_format() {
        let dir = std::env::temp_dir().join(format!(
            "graft-store-evidence-index-legacy-test-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        let store = GraftStore::open(&dir);
        store.init_storage().unwrap();
        fs::write(
            store
                .paths()
                .object_patch_evidence_index()
                .join("patch:legacy.json"),
            r#"["ev:one","ev:two"]"#,
        )
        .unwrap();

        let error = store
            .patch_evidence_index("patch:legacy")
            .unwrap_err()
            .to_string();

        assert!(error.contains("invalid evidence index"), "{error}");
        assert!(
            error.contains("expected evidence refs object with owner and evidence fields"),
            "{error}"
        );

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn evidence_index_rejects_non_schema_json() {
        let dir = std::env::temp_dir().join(format!(
            "graft-store-evidence-index-scalar-test-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        let store = GraftStore::open(&dir);
        store.init_storage().unwrap();
        fs::write(
            store
                .paths()
                .object_patch_evidence_index()
                .join("patch:scalar.json"),
            r#""ev:fake""#,
        )
        .unwrap();

        let error = store
            .patch_evidence_index("patch:scalar")
            .unwrap_err()
            .to_string();

        assert!(error.contains("invalid evidence index"), "{error}");
        assert!(
            error.contains("expected evidence refs object with owner and evidence fields"),
            "{error}"
        );

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn evidence_index_rejects_owner_mismatch() {
        let dir = std::env::temp_dir().join(format!(
            "graft-store-evidence-index-owner-test-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        let store = GraftStore::open(&dir);
        store.init_storage().unwrap();
        fs::write(
            store
                .paths()
                .object_patch_evidence_index()
                .join("patch:actual.json"),
            r#"{"owner":"patch:other","evidence":["ev:one"],"updated_at":null}"#,
        )
        .unwrap();

        let error = store
            .patch_evidence_index("patch:actual")
            .unwrap_err()
            .to_string();

        assert!(error.contains("invalid evidence index"), "{error}");
        assert!(error.contains("owner `patch:other`"), "{error}");

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn stores_relations_promotions_and_refs() {
        let dir =
            std::env::temp_dir().join(format!("graft-store-record-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let store = GraftStore::open(&dir);
        store.init().unwrap();

        let relation = PatchRelation {
            id: RelationId::new("relation:demo"),
            kind: PatchRelationKind::Composes,
            subject: "candidate:demo".to_string(),
            sources: vec!["patch:left".to_string(), "patch:right".to_string()],
            created_at: "now".to_string(),
        };
        store.write_cache_relation(&relation).unwrap();
        assert_eq!(
            store
                .cached_relations_for_subject("candidate:demo")
                .unwrap(),
            vec![relation]
        );
        assert!(store.list_relations().unwrap().is_empty());

        let promotion = PromotionRecord {
            id: PromotionId::new("promotion:demo"),
            patch_id: PatchId::new("patch:demo"),
            target: "main".to_string(),
            dry_run: false,
            status: "recorded".to_string(),
            promoted_at: "now".to_string(),
        };
        store.write_promotion(&promotion).unwrap();
        assert_eq!(store.list_promotions().unwrap(), vec![promotion]);
        assert!(
            store
                .write_ref("graft/patches/patch:demo", "patch:demo")
                .unwrap()
                .exists()
        );

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn init_storage_migrates_legacy_state_dir_to_local() {
        let dir = std::env::temp_dir().join(format!(
            "graft-store-local-dir-migrate-test-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        let legacy = dir.join(".graft").join("state");
        fs::create_dir_all(legacy.join("aliases")).unwrap();
        fs::write(legacy.join("index.sqlite"), b"legacy").unwrap();

        let store = GraftStore::open(&dir);
        store.init_storage().unwrap();

        assert!(!legacy.exists());
        assert!(store.paths().local_root().is_dir());
        assert!(store.paths().index().is_file());
        assert!(store.paths().refs().is_dir());

        let _ = fs::remove_dir_all(&dir);
    }
}
