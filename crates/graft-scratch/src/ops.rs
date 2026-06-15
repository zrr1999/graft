use super::*;

pub(crate) fn base_state_for_ref(store: &GraftStore, base: &VirtualBaseRef) -> Result<StateId> {
    match base {
        VirtualBaseRef::Empty => {
            let empty = TreeSnapshot::new(Vec::new());
            let (tree_id, _) = store.write_tree_snapshot(&empty)?;
            Ok(StateId::GraftTree(tree_id))
        }
        VirtualBaseRef::Tree(id) => Ok(StateId::GraftTree(id.clone())),
        VirtualBaseRef::Candidate(id) => {
            let candidate = store.read_candidate(id.as_str())?;
            Ok(store
                .resolve_application(&candidate.application)?
                .record
                .target_state)
        }
        VirtualBaseRef::Patch(id) => {
            let patch = store.read_patch(id.as_str())?;
            Ok(store
                .resolve_application(&patch.application)?
                .record
                .target_state)
        }
    }
}

pub(crate) fn utf8_text(file: &VirtualFile) -> Result<String> {
    String::from_utf8(file.bytes.clone()).map_err(|_| ScratchError::BinaryFile {
        path: file.path.clone(),
    })
}

pub(crate) fn normalize_path(path: &str) -> Result<String> {
    if path.is_empty() || path.starts_with('/') || path.split('/').any(|part| part.is_empty()) {
        return Err(ScratchError::InvalidPatch(format!("invalid path: {path}")));
    }
    let mut parts = Vec::new();
    for part in path.split('/') {
        if matches!(part, "." | "..") {
            return Err(ScratchError::InvalidPatch(format!("invalid path: {path}")));
        }
        parts.push(part);
    }
    Ok(parts.join("/"))
}

pub(crate) fn upsert_entry(snapshot: &TreeSnapshot, entry: TreeEntry) -> TreeSnapshot {
    let mut entries = snapshot
        .entries
        .iter()
        .filter(|existing| existing.path != entry.path)
        .cloned()
        .collect::<Vec<_>>();
    entries.push(entry);
    TreeSnapshot::new(entries)
}

pub(crate) fn remove_entry(
    snapshot: &TreeSnapshot,
    path: &str,
) -> Result<(TreeSnapshot, TreeEntry)> {
    let path = normalize_path(path)?;
    let mut removed = None;
    let mut entries = Vec::with_capacity(snapshot.entries.len().saturating_sub(1));
    for entry in &snapshot.entries {
        if entry.path == path {
            removed = Some(entry.clone());
        } else {
            entries.push(entry.clone());
        }
    }

    if let Some(removed) = removed {
        return Ok((TreeSnapshot::new(entries), removed));
    }

    let prefix = format!("{path}/");
    if snapshot
        .entries
        .iter()
        .any(|entry| entry.path.starts_with(&prefix))
    {
        return Err(ScratchError::Store(StoreError::VirtualPathIsDirectory(
            path,
        )));
    }

    Err(ScratchError::Store(StoreError::VirtualPathNotFound(path)))
}
