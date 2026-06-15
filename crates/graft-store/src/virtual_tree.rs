use super::*;

impl GraftStore {
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
}
