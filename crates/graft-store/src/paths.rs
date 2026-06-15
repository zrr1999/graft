use std::path::{Path, PathBuf};

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

    pub fn legacy_properties_config(&self) -> PathBuf {
        self.workspace.join("properties")
    }

    pub fn constraints_roto_config(&self) -> PathBuf {
        self.workspace.join("constraints.roto")
    }

    pub fn constraints_lock(&self) -> PathBuf {
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

    pub fn object_constraints(&self) -> PathBuf {
        self.root.join("store").join("public").join("constraint")
    }

    pub fn object_plans(&self) -> PathBuf {
        self.root.join("store").join("public").join("plan")
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

    pub(crate) const LEGACY_LOCAL_DIR: &str = "state";

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
