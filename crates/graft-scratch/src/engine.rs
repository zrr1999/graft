use super::*;
use crate::hashlines::{apply_edits, reject_display_prefixes, render_hashlines};
use crate::ops::{base_state_for_ref, normalize_path, remove_entry, upsert_entry, utf8_text};

impl ScratchEngine {
    pub fn new(store: GraftStore) -> Self {
        Self {
            store,
            states: Mutex::new(HashMap::new()),
            leases: Mutex::new(HashMap::new()),
        }
    }

    pub fn open(&self, base: VirtualBaseRef) -> Result<ScratchId> {
        let base_tree = self.store.virtual_tree(&base)?;
        let base_state = base_state_for_ref(&self.store, &base)?;
        self.open_resolved(base_state, base_tree)
    }

    pub fn open_materialized(&self, base_state: StateId, tree_id: &str) -> Result<ScratchId> {
        let base_tree = self.store.read_tree_snapshot(tree_id)?;
        self.open_resolved(base_state, base_tree)
    }

    fn open_resolved(&self, base_state: StateId, base_tree: TreeSnapshot) -> Result<ScratchId> {
        let tree_id = base_tree.id()?;
        let node = ScratchNode::root(base_state, tree_id);
        let id = scratch_id(&node)?;
        self.states.lock().expect("scratch state poisoned").insert(
            id.clone(),
            ScratchState {
                node,
                base_tree: base_tree.clone(),
                tree: base_tree,
            },
        );
        Ok(id)
    }

    pub fn read(&self, scratch: &ScratchId, path: &str, mode: ReadMode) -> Result<ScratchRead> {
        let state = self.state(scratch)?;
        let file = self.store.virtual_read(&state.tree, path)?;
        let bytes_hash = blake3::hash(&file.bytes).to_hex().to_string();
        let view_hash = file_view_hash(&FileViewHashSeed {
            scratch,
            path: &file.path,
            bytes_hash: &bytes_hash,
        })?;
        let content = match mode {
            ReadMode::Bytes => String::new(),
            ReadMode::Text => utf8_text(&file)?,
            ReadMode::Hashlines => render_hashlines(&utf8_text(&file)?),
        };
        Ok(ScratchRead {
            scratch: scratch.clone(),
            path: file.path,
            mode,
            file_view_hash: view_hash,
            bytes: file.bytes,
            content,
        })
    }

    pub fn write(&self, scratch: &ScratchId, path: &str, bytes: &[u8]) -> Result<ScratchWrite> {
        let parent = self.state(scratch)?;
        let content_hash = self.store.write_blob(bytes)?;
        let path = normalize_path(path)?;
        let tree = upsert_entry(
            &parent.tree,
            TreeEntry {
                path: path.clone(),
                hash: content_hash.clone(),
                size: bytes.len() as u64,
            },
        );
        let tree_id = tree.id()?;
        self.store.write_tree_snapshot(&tree)?;
        let node = ScratchNode::child(
            scratch.clone(),
            parent.node.base_state.clone(),
            CanonicalScratchOp::Write {
                path: path.clone(),
                content_hash: content_hash.clone(),
                size: bytes.len() as u64,
            },
            tree_id,
        );
        let id = scratch_id(&node)?;
        self.states.lock().expect("scratch state poisoned").insert(
            id.clone(),
            ScratchState {
                node,
                base_tree: parent.base_tree,
                tree,
            },
        );
        Ok(ScratchWrite {
            parent: scratch.clone(),
            scratch: id,
            path,
            content_hash,
            size: bytes.len() as u64,
        })
    }

    pub fn edit(
        &self,
        scratch: &ScratchId,
        path: &str,
        edits: Vec<HashlineEdit>,
    ) -> Result<ScratchEdit> {
        let parent = self.state(scratch)?;
        let file = self.store.virtual_read(&parent.tree, path)?;
        let original = utf8_text(&file)?;
        reject_display_prefixes(&edits)?;
        let edited = apply_edits(&original, &edits)?;
        let write = self.write(scratch, &file.path, edited.as_bytes())?;

        let mut states = self.states.lock().expect("scratch state poisoned");
        let child = states
            .get_mut(&write.scratch)
            .ok_or_else(|| ScratchError::UnknownScratch(write.scratch.to_string()))?;
        child.node.op = Some(CanonicalScratchOp::Edit {
            path: file.path.clone(),
            edits,
        });
        let corrected_id = scratch_id(&child.node)?;
        if corrected_id != write.scratch {
            let corrected = child.clone();
            states.remove(&write.scratch);
            states.insert(corrected_id.clone(), corrected);
            return Ok(ScratchEdit {
                parent: scratch.clone(),
                scratch: corrected_id,
                path: file.path,
                updated_anchors: render_hashlines(&edited),
            });
        }

        Ok(ScratchEdit {
            parent: scratch.clone(),
            scratch: write.scratch,
            path: file.path,
            updated_anchors: render_hashlines(&edited),
        })
    }

    pub fn delete(&self, scratch: &ScratchId, path: &str) -> Result<ScratchDelete> {
        let parent = self.state(scratch)?;
        let (tree, removed) = remove_entry(&parent.tree, path)?;
        let tree_id = tree.id()?;
        self.store.write_tree_snapshot(&tree)?;
        let node = ScratchNode::child(
            scratch.clone(),
            parent.node.base_state.clone(),
            CanonicalScratchOp::Delete {
                path: removed.path.clone(),
            },
            tree_id,
        );
        let id = scratch_id(&node)?;
        self.states.lock().expect("scratch state poisoned").insert(
            id.clone(),
            ScratchState {
                node,
                base_tree: parent.base_tree,
                tree,
            },
        );
        Ok(ScratchDelete {
            parent: scratch.clone(),
            scratch: id,
            path: removed.path,
        })
    }

    pub fn capture_tree(
        &self,
        base_state: StateId,
        base_tree_id: &str,
        target_tree_id: &str,
    ) -> Result<ScratchCapture> {
        let base_tree = self.store.read_tree_snapshot(base_tree_id)?;
        let target_tree = self.store.read_tree_snapshot(target_tree_id)?;
        let change = Change::from_snapshots(
            base_state.clone(),
            Some(&base_tree),
            StateId::GraftTree(target_tree_id.to_string()),
            &target_tree,
        );
        let changed_paths = change.changed_paths();
        if changed_paths.is_empty() {
            return Err(ScratchError::EmptyChange);
        }

        let node = ScratchNode::root(base_state, target_tree_id.to_string());
        let id = scratch_id(&node)?;
        self.states.lock().expect("scratch state poisoned").insert(
            id.clone(),
            ScratchState {
                node,
                base_tree,
                tree: target_tree,
            },
        );

        Ok(ScratchCapture {
            scratch: id,
            base_tree: base_tree_id.to_string(),
            target_tree: target_tree_id.to_string(),
            changed_paths,
        })
    }

    pub fn diff(&self, from: &ScratchId, to: &ScratchId) -> Result<ScratchDiff> {
        let from_state = self.state(from)?;
        let to_state = self.state(to)?;
        let change = Change::from_snapshots(
            from_state.node.base_state,
            Some(&from_state.tree),
            to_state.node.base_state,
            &to_state.tree,
        );
        Ok(ScratchDiff {
            from: from.clone(),
            to: to.clone(),
            changed_paths: change.changed_paths(),
        })
    }

    pub fn pin(&self, scratch: &ScratchId) -> Result<ScratchPin> {
        let _ = self.state(scratch)?;
        let mut leases = self.leases.lock().expect("scratch lease state poisoned");
        let lease = format!(
            "lease_{}",
            &blake3::hash(format!("{}:{}", scratch, leases.len()).as_bytes()).to_hex()[..12]
        );
        leases.insert(lease.clone(), scratch.clone());
        let pinned = leases.values().filter(|id| *id == scratch).count();
        Ok(ScratchPin {
            scratch: scratch.clone(),
            lease,
            pinned,
        })
    }

    pub fn unpin(&self, lease: &str) -> Result<ScratchPin> {
        let mut leases = self.leases.lock().expect("scratch lease state poisoned");
        let scratch = leases
            .remove(lease)
            .ok_or_else(|| ScratchError::UnknownLease(lease.to_string()))?;
        let pinned = leases.values().filter(|id| *id == &scratch).count();
        Ok(ScratchPin {
            scratch,
            lease: lease.to_string(),
            pinned,
        })
    }

    pub fn drop_scratch(&self, scratch: &ScratchId) -> Result<bool> {
        let leases = self.leases.lock().expect("scratch lease state poisoned");
        if leases.values().any(|id| id == scratch) {
            return Err(ScratchError::ScratchPinned(scratch.to_string()));
        }
        drop(leases);
        self.states
            .lock()
            .expect("scratch state poisoned")
            .remove(scratch)
            .map(|_| true)
            .ok_or_else(|| ScratchError::UnknownScratch(scratch.to_string()))
    }

    pub fn candidate_from_scratch(
        &self,
        scratch: &ScratchId,
        constraint: Constraint,
        producer: impl Into<String>,
        message: Option<String>,
    ) -> Result<CandidateFromScratch> {
        let state = self.state(scratch)?;
        let target_tree_id = state.tree.id()?;
        self.store.write_tree_snapshot(&state.tree)?;
        let target_state = StateId::GraftTree(target_tree_id);
        let materialized = materialize_application(
            state.node.base_state.clone(),
            Some(&state.base_tree),
            target_state,
            &state.tree,
        )
        .map_err(ScratchError::Core)?;
        let changed_paths = materialized.change.changed_paths();
        let application = self
            .store
            .write_materialized_application(&materialized)
            .map_err(ScratchError::Store)?;
        let mut candidate = GraftCandidate {
            id: graft_core::CandidateId::new("candidate:pending"),
            application,
            constraint,
            provenance: graft_core::Provenance::now(producer, message),
        };
        candidate.id = candidate_id(&candidate)?;
        self.store.write_candidate(&candidate)?;
        self.store
            .write_candidate_evidence_index(candidate.id.as_str(), &[])?;
        Ok(CandidateFromScratch {
            scratch: scratch.clone(),
            candidate: candidate.id,
            changed_paths,
        })
    }

    pub fn tree_snapshot(&self, scratch: &ScratchId) -> Result<TreeSnapshot> {
        Ok(self.state(scratch)?.tree)
    }

    pub fn store(&self) -> &GraftStore {
        &self.store
    }

    pub(crate) fn state(&self, scratch: &ScratchId) -> Result<ScratchState> {
        self.states
            .lock()
            .expect("scratch state poisoned")
            .get(scratch)
            .cloned()
            .ok_or_else(|| ScratchError::ScratchLost(scratch.to_string()))
    }
}
