use std::fs;
use std::path::{Component, Path, PathBuf};

use graft_core::{
    CandidateId, ChangeSet, EvidenceRecord, GraftCandidate, PatchId, PatchRecord, PatchRelation,
    PromotionRecord, PropertyRef, StateId, TreeEntry, TreeSnapshot,
};
use rusqlite::Connection;

pub mod lock;
pub use lock::WriteLock;

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("sqlite error: {0}")]
    Sqlite(#[from] rusqlite::Error),
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
    #[error(
        "another graft writer holds the lock at {} - only one graftd may write `.graft/` at a time",
        path.display()
    )]
    Locked { path: PathBuf },
}

pub type Result<T> = std::result::Result<T, StoreError>;

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum VirtualBaseRef {
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
struct EvidenceRefs {
    owner: String,
    evidence: Vec<String>,
    updated_at: Option<String>,
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

    pub fn properties_lock(&self) -> PathBuf {
        self.workspace.join("graft.lock")
    }

    pub fn graft_config(&self) -> PathBuf {
        self.root.join("config.toml")
    }

    pub fn state_cwd(&self) -> PathBuf {
        self.root.join("state").join("cwd")
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

    pub fn cache_tmp(&self) -> PathBuf {
        self.root.join("run").join("tmp")
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

    pub fn index(&self) -> PathBuf {
        self.root.join("state").join("index.sqlite")
    }

    pub fn refs(&self) -> PathBuf {
        self.root.join("state").join("aliases")
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
        let properties_config_existed = properties_config_path.exists();
        if !properties_config_existed {
            write_default_properties_config(&properties_config_path)?;
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
        fs::create_dir_all(self.paths.object_blobs())?;
        fs::create_dir_all(self.paths.object_trees())?;
        fs::create_dir_all(self.paths.object_changes())?;
        fs::create_dir_all(self.paths.object_patches())?;
        fs::create_dir_all(self.paths.object_evidence())?;
        fs::create_dir_all(self.paths.object_candidate_evidence_index())?;
        fs::create_dir_all(self.paths.object_patch_evidence_index())?;
        fs::create_dir_all(self.paths.cache_candidates())?;
        fs::create_dir_all(self.paths.cache_evidence())?;
        fs::create_dir_all(self.paths.cache_trials())?;
        fs::create_dir_all(self.paths.cache_relations())?;
        fs::create_dir_all(self.paths.cache_worktrees())?;
        fs::create_dir_all(self.paths.cache_tmp())?;
        fs::create_dir_all(self.paths.registry_patches())?;
        fs::create_dir_all(self.paths.registry_evidence())?;
        fs::create_dir_all(self.paths.registry_relations())?;
        fs::create_dir_all(self.paths.registry_promotions())?;
        fs::create_dir_all(self.paths.refs().join("drafts"))?;
        fs::create_dir_all(self.paths.refs().join("registry"))?;
        fs::create_dir_all(self.paths.materialized_refs())?;
        let state_cwd = self.paths.state_cwd();
        if !state_cwd.exists() {
            fs::write(state_cwd, "")?;
        }
        self.init_index()
    }

    pub fn capture_worktree_snapshot(&self, worktree: impl AsRef<Path>) -> Result<TreeSnapshot> {
        let worktree = worktree.as_ref();
        fs::create_dir_all(self.paths.object_blobs())?;
        let mut entries = Vec::new();
        collect_tree_entries(worktree, worktree, &self.paths.object_blobs(), &mut entries)?;
        Ok(TreeSnapshot::new(entries))
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
            let Some(hash) = path.file_name().and_then(|value| value.to_str()) else {
                continue;
            };
            blobs.push((hash.to_string(), fs::read(path)?));
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
            VirtualBaseRef::Tree(id) => self.read_tree_snapshot(id),
            VirtualBaseRef::Candidate(id) => {
                let candidate = self.read_candidate(id.as_str())?;
                self.virtual_tree_for_state(&candidate.target_state)
            }
            VirtualBaseRef::Patch(id) => {
                let patch = self.read_patch(id.as_str())?;
                self.virtual_tree_for_state(&patch.target_state)
            }
        }
    }

    pub fn virtual_tree_for_state(&self, state: &StateId) -> Result<TreeSnapshot> {
        match state {
            StateId::GraftTree(id) | StateId::GitTree(id) => self.read_tree_snapshot(id),
            StateId::RepoTree(repo) => Err(StoreError::UnsupportedVirtualBase(repo.display_ref())),
        }
    }

    pub fn read_cwd_state(&self) -> Result<Option<StateId>> {
        let content = match fs::read_to_string(self.paths.state_cwd()) {
            Ok(content) => content,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(StoreError::Io(error)),
        };
        let trimmed = content.trim();
        if trimmed.is_empty() {
            Ok(None)
        } else {
            Ok(Some(serde_json::from_str(trimmed)?))
        }
    }

    pub fn write_cwd_state(&self, state: &StateId) -> Result<()> {
        if let Some(parent) = self.paths.state_cwd().parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(self.paths.state_cwd(), serde_json::to_string_pretty(state)?)?;
        Ok(())
    }

    pub fn materialize_workspace_view(&self, snapshot: &TreeSnapshot) -> Result<()> {
        clear_workspace_view(self.paths.workspace())?;
        for entry in &snapshot.entries {
            let path = materialized_path(self.paths.workspace(), &entry.path)?;
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent)?;
            }
            fs::write(path, self.read_blob(&entry.hash)?)?;
        }
        Ok(())
    }

    pub fn materialize_tree_snapshot(
        &self,
        snapshot: &TreeSnapshot,
        destination: impl AsRef<Path>,
    ) -> Result<()> {
        let destination = destination.as_ref();
        if destination.exists() {
            if destination.is_dir() {
                fs::remove_dir_all(destination)?;
            } else {
                fs::remove_file(destination)?;
            }
        }
        fs::create_dir_all(destination)?;
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
        read_named_json_records(&self.paths.object_trees())
    }

    pub fn write_change(&self, change: &ChangeSet) -> Result<(graft_core::ChangeId, PathBuf)> {
        fs::create_dir_all(self.paths.object_changes())?;
        let id = change.id().map_err(StoreError::Core)?;
        let path = self.paths.object_changes().join(format!("{id}.json"));
        write_json(&path, change)?;
        Ok((id, path))
    }

    pub fn read_change(&self, id: &str) -> Result<ChangeSet> {
        read_json(&self.paths.object_changes().join(format!("{id}.json")))
    }

    pub fn list_change_objects(&self) -> Result<Vec<(String, ChangeSet)>> {
        read_named_json_records(&self.paths.object_changes())
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
        read_json(&self.paths.cache_candidates().join(format!("{id}.json")))
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
        let refs = EvidenceRefs {
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
        fs::create_dir_all(self.paths.object_patch_evidence_index())?;
        let refs = EvidenceRefs {
            owner: patch.to_string(),
            evidence: index.clone(),
            updated_at: Some(time::OffsetDateTime::now_utc().to_string()),
        };
        write_json(
            &self
                .paths
                .object_patch_evidence_index()
                .join(format!("{patch}.json")),
            &refs,
        )?;
        Ok(index)
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
        let indexed = self.candidate_evidence_records(subject)?;
        if indexed.is_empty() {
            read_evidence_records(&self.paths.cache_evidence(), subject)
        } else {
            Ok(indexed)
        }
    }

    pub fn registry_evidence_for_subject(&self, subject: &str) -> Result<Vec<EvidenceRecord>> {
        let indexed = self.patch_evidence_records(subject)?;
        if indexed.is_empty() {
            read_evidence_records(&self.paths.registry_evidence(), subject)
        } else {
            Ok(indexed)
        }
    }

    pub fn list_registry_evidence(&self) -> Result<Vec<EvidenceRecord>> {
        read_json_records(&self.paths.object_evidence())
    }

    pub fn write_registry_evidence(&self, evidence: &EvidenceRecord) -> Result<PathBuf> {
        self.write_evidence(evidence)
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
        read_json(&self.paths.registry_patches().join(format!("{id}.json")))
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

    fn init_index(&self) -> Result<()> {
        if let Some(parent) = self.paths.index().parent() {
            fs::create_dir_all(parent)?;
        }
        let conn = Connection::open(self.paths.index())?;
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
        let conn = Connection::open(self.paths.index())?;
        conn.execute(
            "INSERT OR REPLACE INTO patches (patch_id, base_state, target_state, admitted_at)
             VALUES (?1, ?2, ?3, ?4)",
            (
                patch.id.to_string(),
                serde_json::to_string(&patch.base_state)?,
                serde_json::to_string(&patch.target_state)?,
                patch.admitted_at.clone(),
            ),
        )?;
        for property in &patch.properties {
            conn.execute(
                "INSERT OR REPLACE INTO patch_properties (patch_id, property) VALUES (?1, ?2)",
                (patch.id.to_string(), serde_json::to_string(property)?),
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

const DEFAULT_CONFIG: &str = r#"[create]
default_base = "HEAD"
default_mode = "cache-only"

[admission]
base_properties = ["ValidPatch"]

[promotion]
required_properties = ["ValidPatch"]
"#;

const DEFAULT_PROPERTIES_CONFIG: &str = r#"[[properties]]
name = "ValidPatch"

[properties.query]
kind = "change"

[properties.evaluator]
kind = "builtin"
name = "valid_patch"

[properties.evaluator.options]

[properties.judge]
kind = "bool_true"

[[properties]]
name = "NoModelWeightChange"

[properties.query]
kind = "files"
include = ["*.pt", "*.pth", "*.onnx", "*.safetensors", "*.ckpt", "*.h5", "*pytorch_model.bin"]
exclude = []

[properties.evaluator]
kind = "builtin"
name = "paths_none_match"

[properties.evaluator.options]

[properties.judge]
kind = "bool_true"
"#;

fn write_default_properties_config(dir: &Path) -> Result<()> {
    fs::create_dir_all(dir)?;
    for chunk in DEFAULT_PROPERTIES_CONFIG.split("[[properties]]") {
        let chunk = chunk.trim();
        if chunk.is_empty() {
            continue;
        }
        let mut name = None;
        let mut lines = Vec::new();
        for line in chunk.lines() {
            if name.is_none()
                && let Some(rest) = line.strip_prefix("name = ")
            {
                name = Some(rest.trim().trim_matches('"').to_string());
                continue;
            }
            lines.push(line.replace("[properties.", "["));
        }
        let Some(name) = name else {
            continue;
        };
        fs::write(
            dir.join(format!("{name}.toml")),
            format!("{}\n", lines.join("\n").trim()),
        )?;
    }
    Ok(())
}

fn write_json(path: &Path, value: &impl serde::Serialize) -> Result<()> {
    let bytes = serde_json::to_vec_pretty(value)?;
    fs::write(path, [bytes, b"\n".to_vec()].concat())?;
    Ok(())
}

fn read_evidence_index(dir: &Path, subject: &str) -> Result<Vec<String>> {
    let path = dir.join(format!("{subject}.json"));
    let value: serde_json::Value = match read_json(&path) {
        Ok(value) => value,
        Err(StoreError::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(Vec::new());
        }
        Err(error) => return Err(error),
    };
    if value.is_array() {
        return Ok(serde_json::from_value(value)?);
    }
    if value.is_object() {
        let refs: EvidenceRefs = serde_json::from_value(value)?;
        return Ok(refs.evidence);
    }
    Ok(Vec::new())
}

fn append_unique_index(dir: &Path, subject: &str, evidence: &str) -> Result<()> {
    fs::create_dir_all(dir)?;
    let path = dir.join(format!("{subject}.json"));
    let mut ids = read_evidence_index(dir, subject)?;
    if !ids.iter().any(|id| id == evidence) {
        ids.push(evidence.to_string());
    }
    let refs = EvidenceRefs {
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

fn read_named_json_records<T: serde::de::DeserializeOwned>(
    path: &Path,
) -> Result<Vec<(String, T)>> {
    if !path.exists() {
        return Ok(Vec::new());
    }
    let paths = json_paths(path)?;
    paths
        .into_iter()
        .filter_map(|path| {
            let id = path
                .file_stem()
                .and_then(|value| value.to_str())
                .map(ToString::to_string)?;
            Some((id, path))
        })
        .map(|(id, path)| read_json(&path).map(|record| (id, record)))
        .collect()
}

fn read_evidence_records(path: &Path, subject: &str) -> Result<Vec<EvidenceRecord>> {
    if !path.exists() {
        return Ok(Vec::new());
    }
    let mut records = Vec::new();
    let paths = json_paths(path)?;
    for path in paths {
        let record: EvidenceRecord = read_json(&path)?;
        if record.subject == subject {
            records.push(record);
        }
    }
    Ok(records)
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
        let file_name = file_name.to_string_lossy();
        let file_type = entry.file_type()?;
        if file_type.is_dir() {
            if should_skip_dir(file_name.as_ref()) {
                continue;
            }
            collect_tree_entries(root, &path, blob_dir, entries)?;
        } else if file_type.is_file() {
            if should_skip_file(file_name.as_ref()) {
                continue;
            }
            let relative = normalize_relative_path(root, &path);
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

fn should_skip_dir(name: &str) -> bool {
    matches!(name, ".git" | ".graft" | ".spark" | "target" | "properties")
}

fn should_skip_file(name: &str) -> bool {
    matches!(name, "graft.toml" | "graft.lock")
}

fn clear_workspace_view(root: &Path) -> Result<()> {
    if !root.exists() {
        return Ok(());
    }
    let mut children = Vec::new();
    for entry in fs::read_dir(root)? {
        children.push(entry?);
    }
    children.sort_by_key(|entry| entry.path());
    for entry in children {
        let path = entry.path();
        let file_name = entry.file_name();
        let file_name = file_name.to_string_lossy();
        let file_type = entry.file_type()?;
        if file_type.is_dir() {
            if should_skip_dir(file_name.as_ref()) {
                continue;
            }
            fs::remove_dir_all(path)?;
        } else if file_type.is_file() {
            if should_skip_file(file_name.as_ref()) {
                continue;
            }
            fs::remove_file(path)?;
        }
    }
    Ok(())
}

fn normalize_virtual_path(path: &str) -> Result<String> {
    if path.is_empty() {
        return Err(StoreError::InvalidPath(path.to_string()));
    }
    let mut parts = Vec::new();
    for component in Path::new(path).components() {
        match component {
            Component::Normal(value) => parts.push(value.to_string_lossy().into_owned()),
            Component::CurDir
            | Component::ParentDir
            | Component::RootDir
            | Component::Prefix(_) => {
                return Err(StoreError::InvalidPath(path.to_string()));
            }
        }
    }
    if parts.is_empty() {
        return Err(StoreError::InvalidPath(path.to_string()));
    }
    Ok(parts.join("/"))
}

fn materialized_path(root: &Path, relative: &str) -> Result<PathBuf> {
    let mut path = root.to_path_buf();
    let mut saw_component = false;
    for component in Path::new(relative).components() {
        match component {
            Component::Normal(value) => {
                path.push(value);
                saw_component = true;
            }
            _ => return Err(StoreError::InvalidPath(relative.to_string())),
        }
    }
    if saw_component {
        Ok(path)
    } else {
        Err(StoreError::InvalidPath(relative.to_string()))
    }
}

fn normalize_relative_path(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .components()
        .map(|component| component.as_os_str().to_string_lossy().into_owned())
        .collect::<Vec<String>>()
        .join("/")
}

#[cfg(test)]
mod tests {
    use super::*;
    use graft_core::{
        ChangeRef, PatchId, PatchRelation, PatchRelationKind, PromotionId, PromotionRecord,
        PropertyId, Provenance, RelationId, StateId,
    };

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

        assert!(dir.join("graft.toml").exists());
        assert!(dir.join("properties").join("ValidPatch.toml").exists());
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
    fn virtual_read_reads_blob_by_snapshot_path() {
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

        let file = store.virtual_read(&snapshot, "src//lib.rs").unwrap();

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

        let candidate = GraftCandidate {
            id: graft_core::CandidateId::new("candidate:demo"),
            base_state: StateId::GraftTree("tree:base".to_string()),
            target_state: StateId::GraftTree(tree_id.clone()),
            change: ChangeRef::InlineSummary("demo".to_string()),
            expected: Vec::new(),
            provenance: Provenance {
                producer: "test".to_string(),
                message: None,
                created_at: "now".to_string(),
            },
        };
        store.write_candidate(&candidate).unwrap();

        let patch = PatchRecord {
            id: PatchId::new("patch:demo"),
            base_state: StateId::GraftTree("tree:base".to_string()),
            target_state: StateId::GraftTree(tree_id.clone()),
            change: ChangeRef::InlineSummary("demo".to_string()),
            properties: Vec::new(),
            provenance: Provenance {
                producer: "test".to_string(),
                message: None,
                created_at: "now".to_string(),
            },
            admitted_at: "now".to_string(),
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
    fn materialization_rejects_paths_that_escape_destination() {
        let dir = std::env::temp_dir().join(format!(
            "graft-store-materialize-path-test-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        let store = GraftStore::open(&dir);
        store.init_storage().unwrap();
        let hash = store.write_blob(b"demo\n").unwrap();
        let snapshot = TreeSnapshot::new(vec![TreeEntry {
            path: "../escape".to_string(),
            hash,
            size: 5,
        }]);

        assert!(matches!(
            store.materialize_tree_snapshot(&snapshot, dir.join("out")),
            Err(StoreError::InvalidPath(_))
        ));

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
        let patch = PatchRecord {
            id: PatchId::new("patch:demo"),
            base_state: StateId::GitTree("base".to_string()),
            target_state: StateId::GraftTree("target".to_string()),
            change: ChangeRef::InlineSummary("demo".to_string()),
            properties: vec![property.clone()],
            provenance: Provenance {
                producer: "test".to_string(),
                message: None,
                created_at: "now".to_string(),
            },
            admitted_at: "now".to_string(),
        };
        store.write_patch(&patch).unwrap();

        assert_eq!(
            store.search_patches_by_property(&property).unwrap(),
            vec!["patch:demo".to_string()]
        );

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn copies_candidate_evidence_index_to_patch_without_rewriting_evidence() {
        let dir = std::env::temp_dir().join(format!(
            "graft-store-evidence-index-test-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        let store = GraftStore::open(&dir);
        store.init_storage().unwrap();

        let property = PropertyId::new("property:indexcopy");
        let evidence = EvidenceRecord::passed("candidate:demo", property, "test-verifier").unwrap();
        let evidence_id = evidence.id.to_string();
        store.write_evidence(&evidence).unwrap();
        store
            .append_candidate_evidence_index("candidate:demo", &evidence_id)
            .unwrap();

        let copied = store
            .copy_candidate_evidence_index_to_patch("candidate:demo", "patch:demo")
            .unwrap();

        assert_eq!(copied, vec![evidence_id.clone()]);
        assert_eq!(store.patch_evidence_index("patch:demo").unwrap(), copied);
        assert_eq!(
            store.patch_evidence_records("patch:demo").unwrap(),
            vec![evidence]
        );
        assert_eq!(
            read_json_records::<EvidenceRecord>(&store.paths().object_evidence())
                .unwrap()
                .len(),
            1
        );

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
}
