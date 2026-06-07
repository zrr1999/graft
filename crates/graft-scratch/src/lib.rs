pub mod wire;

use std::collections::HashMap;
use std::sync::Mutex;

use graft_core::{
    CanonicalScratchOp, Change, FileViewHash, FileViewHashSeed, GraftCandidate, HashlineEdit,
    ScopedPropertyRef, ScratchId, ScratchNode, StateId, TreeEntry, TreeSnapshot, candidate_id,
    file_view_hash, materialize_application, scratch_id,
};
use graft_store::{GraftStore, StoreError, VirtualBaseRef, VirtualFile};

const HASHLINE_ALPHABET: &[u8; 16] = b"ZPMQVRWSNKTXJBYH";

#[derive(Debug, thiserror::Error)]
pub enum ScratchError {
    #[error("store error: {0}")]
    Store(#[from] StoreError),
    #[error("core error: {0}")]
    Core(#[from] graft_core::CoreError),
    #[error("scratch not found: {0}")]
    UnknownScratch(String),
    #[error("scratch state was lost; retry with --base: {0}")]
    ScratchLost(String),
    #[error("scratch has no changes to turn into a candidate")]
    EmptyChange,
    #[error("path is not valid utf-8 text: {path}")]
    BinaryFile { path: String },
    #[error("stale anchor at line {line}: expected {expected_hash}, got {actual_hash}")]
    StaleAnchor {
        line: u64,
        expected_hash: String,
        actual_hash: String,
        fresh_anchors: String,
    },
    #[error("replace_text matched {matches} occurrences; expected exactly 1")]
    AmbiguousText { matches: usize },
    #[error("invalid edit payload: {0}")]
    InvalidPatch(String),
    #[error("line out of range: {0}")]
    LineOutOfRange(u64),
    #[error("scratch is pinned: {0}")]
    ScratchPinned(String),
    #[error("unknown lease: {0}")]
    UnknownLease(String),
}

pub type Result<T> = std::result::Result<T, ScratchError>;

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ReadMode {
    Bytes,
    Text,
    Hashlines,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ScratchRead {
    pub scratch: ScratchId,
    pub path: String,
    pub mode: ReadMode,
    pub file_view_hash: FileViewHash,
    pub bytes: Vec<u8>,
    pub content: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ScratchWrite {
    pub parent: ScratchId,
    pub scratch: ScratchId,
    pub path: String,
    pub content_hash: String,
    pub size: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ScratchEdit {
    pub parent: ScratchId,
    pub scratch: ScratchId,
    pub path: String,
    pub updated_anchors: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ScratchDelete {
    pub parent: ScratchId,
    pub scratch: ScratchId,
    pub path: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ScratchCapture {
    pub scratch: ScratchId,
    pub base_tree: String,
    pub target_tree: String,
    pub changed_paths: Vec<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CandidateFromScratch {
    pub scratch: ScratchId,
    pub candidate: graft_core::CandidateId,
    pub changed_paths: Vec<String>,
}

#[deprecated(note = "use CandidateFromScratch")]
pub type ScratchPromotion = CandidateFromScratch;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ScratchDiff {
    pub from: ScratchId,
    pub to: ScratchId,
    pub changed_paths: Vec<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ScratchPin {
    pub scratch: ScratchId,
    pub lease: String,
    pub pinned: usize,
}

#[derive(Clone, Debug)]
struct ScratchState {
    node: ScratchNode,
    base_tree: TreeSnapshot,
    tree: TreeSnapshot,
}

#[derive(Debug)]
pub struct ScratchEngine {
    store: GraftStore,
    states: Mutex<HashMap<ScratchId, ScratchState>>,
    leases: Mutex<HashMap<String, ScratchId>>,
}

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
        expected: Vec<ScopedPropertyRef>,
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
            expected,
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

    #[deprecated(note = "use candidate_from_scratch")]
    pub fn promote(
        &self,
        scratch: &ScratchId,
        expected: Vec<ScopedPropertyRef>,
        producer: impl Into<String>,
        message: Option<String>,
    ) -> Result<CandidateFromScratch> {
        self.candidate_from_scratch(scratch, expected, producer, message)
    }

    pub fn store(&self) -> &GraftStore {
        &self.store
    }

    fn state(&self, scratch: &ScratchId) -> Result<ScratchState> {
        self.states
            .lock()
            .expect("scratch state poisoned")
            .get(scratch)
            .cloned()
            .ok_or_else(|| ScratchError::ScratchLost(scratch.to_string()))
    }
}

pub fn render_hashlines(text: &str) -> String {
    logical_lines(text)
        .iter()
        .enumerate()
        .map(|(idx, line)| format!("{}#{}:{}\n", idx + 1, line_hash(idx + 1, line), line))
        .collect()
}

pub fn line_hash(line_number: usize, line: &str) -> String {
    let mut hasher = blake3::Hasher::new();
    if !line.chars().any(|ch| ch.is_alphanumeric()) {
        hasher.update(line_number.to_string().as_bytes());
        hasher.update(b"\0");
    }
    hasher.update(line.as_bytes());
    let digest = hasher.finalize();
    let bytes = digest.as_bytes();
    let first = HASHLINE_ALPHABET[(bytes[0] & 0x0f) as usize] as char;
    let second = HASHLINE_ALPHABET[(bytes[1] & 0x0f) as usize] as char;
    format!("{first}{second}")
}

fn base_state_for_ref(store: &GraftStore, base: &VirtualBaseRef) -> Result<StateId> {
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

fn utf8_text(file: &VirtualFile) -> Result<String> {
    String::from_utf8(file.bytes.clone()).map_err(|_| ScratchError::BinaryFile {
        path: file.path.clone(),
    })
}

fn normalize_path(path: &str) -> Result<String> {
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

fn upsert_entry(snapshot: &TreeSnapshot, entry: TreeEntry) -> TreeSnapshot {
    let mut entries = snapshot
        .entries
        .iter()
        .filter(|existing| existing.path != entry.path)
        .cloned()
        .collect::<Vec<_>>();
    entries.push(entry);
    TreeSnapshot::new(entries)
}

fn remove_entry(snapshot: &TreeSnapshot, path: &str) -> Result<(TreeSnapshot, TreeEntry)> {
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

fn logical_lines(text: &str) -> Vec<String> {
    let text = text.strip_suffix('\n').unwrap_or(text);
    if text.is_empty() {
        return Vec::new();
    }
    text.split('\n')
        .map(|line| line.strip_suffix('\r').unwrap_or(line).to_string())
        .collect()
}

fn reject_display_prefixes(edits: &[HashlineEdit]) -> Result<()> {
    fn check_line(line: &str) -> Result<()> {
        if looks_like_display_prefixed_line(line) {
            return Err(ScratchError::InvalidPatch(
                "replacement line contains LINE#HASH display prefix".to_string(),
            ));
        }
        Ok(())
    }

    for edit in edits {
        match edit {
            HashlineEdit::ReplaceLine { new, .. } => check_line(new)?,
            HashlineEdit::ReplaceRange { new_lines, .. }
            | HashlineEdit::InsertAfter { new_lines, .. }
            | HashlineEdit::InsertBefore { new_lines, .. } => {
                for line in new_lines {
                    check_line(line)?;
                }
            }
            HashlineEdit::ReplaceText { new_text, .. } => {
                for line in new_text.lines() {
                    check_line(line)?;
                }
            }
        }
    }
    Ok(())
}

fn looks_like_display_prefixed_line(line: &str) -> bool {
    let Some((line_number, rest)) = line.split_once('#') else {
        return false;
    };
    !line_number.is_empty()
        && line_number.chars().all(|ch| ch.is_ascii_digit())
        && rest.len() >= 3
        && rest.as_bytes().get(2) == Some(&b':')
        && rest[..2]
            .chars()
            .all(|ch| (HASHLINE_ALPHABET.as_slice()).contains(&(ch as u8)))
}

fn apply_edits(text: &str, edits: &[HashlineEdit]) -> Result<String> {
    let mut lines = logical_lines(text);
    for edit in edits {
        match edit {
            HashlineEdit::ReplaceLine {
                line,
                hash,
                old,
                new,
            } => {
                let idx = checked_line_index(*line, lines.len())?;
                verify_anchor(*line, hash, old, &lines[idx], &lines)?;
                lines[idx] = new.clone();
            }
            HashlineEdit::ReplaceRange {
                start_line,
                start_hash,
                end_line,
                end_hash,
                new_lines,
            } => {
                if start_line > end_line {
                    return Err(ScratchError::InvalidPatch(
                        "replace_range start_line is after end_line".to_string(),
                    ));
                }
                let start_idx = checked_line_index(*start_line, lines.len())?;
                let end_idx = checked_line_index(*end_line, lines.len())?;
                let start_actual_hash = line_hash(*start_line as usize, &lines[start_idx]);
                let end_actual_hash = line_hash(*end_line as usize, &lines[end_idx]);
                let start_stale = &start_actual_hash != start_hash;
                let end_stale = &end_actual_hash != end_hash;
                if start_stale || end_stale {
                    let stale_line = if start_stale { *start_line } else { *end_line };
                    let stale_expected = if start_stale {
                        start_hash.clone()
                    } else {
                        end_hash.clone()
                    };
                    let stale_actual = if start_stale {
                        start_actual_hash
                    } else {
                        end_actual_hash
                    };
                    // Always include both endpoints in the fresh-anchor
                    // block so a single-end stale does not erase the
                    // surviving end’s anchor; this is what callers need to
                    // re-anchor a range edit without re-reading the file.
                    let fresh_anchors =
                        render_range_context(&lines, *start_line as usize, *end_line as usize);
                    return Err(ScratchError::StaleAnchor {
                        line: stale_line,
                        expected_hash: stale_expected,
                        actual_hash: stale_actual,
                        fresh_anchors,
                    });
                }
                lines.splice(start_idx..=end_idx, new_lines.clone());
            }
            HashlineEdit::InsertAfter {
                line,
                hash,
                new_lines,
            } => {
                let idx = checked_line_index(*line, lines.len())?;
                let old = lines[idx].clone();
                verify_anchor(*line, hash, &old, &old, &lines)?;
                lines.splice(idx + 1..idx + 1, new_lines.clone());
            }
            HashlineEdit::InsertBefore {
                line,
                hash,
                new_lines,
            } => {
                let idx = checked_line_index(*line, lines.len())?;
                let old = lines[idx].clone();
                verify_anchor(*line, hash, &old, &old, &lines)?;
                lines.splice(idx..idx, new_lines.clone());
            }
            HashlineEdit::ReplaceText { old_text, new_text } => {
                let matches = text_matches(&lines.join("\n"), old_text);
                if matches != 1 {
                    return Err(ScratchError::AmbiguousText { matches });
                }
                lines = logical_lines(&lines.join("\n").replace(old_text, new_text));
            }
        }
    }
    Ok(if lines.is_empty() {
        String::new()
    } else {
        format!("{}\n", lines.join("\n"))
    })
}

fn checked_line_index(line: u64, len: usize) -> Result<usize> {
    let idx = line
        .checked_sub(1)
        .ok_or(ScratchError::LineOutOfRange(line))? as usize;
    if idx >= len {
        return Err(ScratchError::LineOutOfRange(line));
    }
    Ok(idx)
}

fn verify_anchor(
    line: u64,
    expected_hash: &str,
    expected_text: &str,
    actual_text: &str,
    lines: &[String],
) -> Result<()> {
    let actual_hash = line_hash(line as usize, actual_text);
    if actual_hash != expected_hash || actual_text != expected_text {
        return Err(ScratchError::StaleAnchor {
            line,
            expected_hash: expected_hash.to_string(),
            actual_hash,
            fresh_anchors: render_context(lines, line as usize),
        });
    }
    Ok(())
}

fn render_context(lines: &[String], target_line: usize) -> String {
    let start = target_line.saturating_sub(2).max(1);
    let end = (target_line + 1).min(lines.len());
    (start..=end)
        .map(|line_number| {
            let text = &lines[line_number - 1];
            let marker = if line_number == target_line {
                ">>> "
            } else {
                ""
            };
            format!(
                "{marker}{line_number}#{}:{text}\n",
                line_hash(line_number, text)
            )
        })
        .collect()
}

/// Render fresh anchors for a `replace_range` edit, marking both endpoints
/// with `>>>` so callers can re-anchor without losing the surviving end when
/// only one end has drifted. Returns a single block, not two, when start and
/// end are close enough that their context windows overlap.
fn render_range_context(lines: &[String], start_line: usize, end_line: usize) -> String {
    if lines.is_empty() {
        return String::new();
    }
    let start_window_lo = start_line.saturating_sub(2).max(1);
    let start_window_hi = (start_line + 1).min(lines.len());
    let end_window_lo = end_line.saturating_sub(2).max(1);
    let end_window_hi = (end_line + 1).min(lines.len());
    if start_window_hi + 1 >= end_window_lo {
        // Windows overlap: merge into one contiguous block, marking both
        // start and end with `>>>`.
        let lo = start_window_lo.min(end_window_lo);
        let hi = start_window_hi.max(end_window_hi);
        return (lo..=hi)
            .map(|n| render_anchor_line(lines, n, n == start_line || n == end_line))
            .collect();
    }
    let mut out = String::new();
    for n in start_window_lo..=start_window_hi {
        out.push_str(&render_anchor_line(lines, n, n == start_line));
    }
    out.push_str("...\n");
    for n in end_window_lo..=end_window_hi {
        out.push_str(&render_anchor_line(lines, n, n == end_line));
    }
    out
}

fn render_anchor_line(lines: &[String], line_number: usize, is_target: bool) -> String {
    let text = &lines[line_number - 1];
    let marker = if is_target { ">>> " } else { "" };
    format!(
        "{marker}{line_number}#{}:{text}\n",
        line_hash(line_number, text)
    )
}

fn text_matches(haystack: &str, needle: &str) -> usize {
    if needle.is_empty() {
        return 0;
    }
    haystack.match_indices(needle).count()
}

#[cfg(test)]
mod tests {
    use super::*;
    use graft_core::{
        FileChangeKind, PropertyId, PropertyRef, PropertyScope, ScopedPropertyRef, TreeEntry,
        TreeSnapshot,
    };

    fn temp_dir(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("graft-scratch-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    fn seeded_engine(name: &str) -> (std::path::PathBuf, ScratchEngine, String) {
        let dir = temp_dir(name);
        let store = GraftStore::open(&dir);
        store.init().unwrap();
        let hash = store
            .write_blob(b"pub fn hello() {\n    println!(\"hello\");\n}\n")
            .unwrap();
        let snapshot = TreeSnapshot::new(vec![TreeEntry {
            path: "src/lib.rs".to_string(),
            hash,
            size: 43,
        }]);
        let (tree_id, _) = store.write_tree_snapshot(&snapshot).unwrap();
        (dir, ScratchEngine::new(store), tree_id)
    }

    #[test]
    fn hashline_rendering_uses_expected_shape_and_alphabet() {
        let rendered = render_hashlines("hello\n!!!\n");
        assert!(rendered.contains("1#"));
        assert!(rendered.contains("2#"));
        for hash in rendered
            .lines()
            .filter_map(|line| line.split_once('#'))
            .map(|(_, rest)| &rest[..2])
        {
            assert_eq!(hash.len(), 2);
            assert!(
                hash.chars()
                    .all(|ch| HASHLINE_ALPHABET.contains(&(ch as u8)))
            );
        }
    }

    /// Lock the hashline algorithm to a stable byte form across versions.
    ///
    /// The combination of (line number, content) -> 2-character anchor must
    /// stay stable so existing edits and stale-anchor diagnostics remain
    /// reproducible. `line_hash` is also content-addressed by line number for
    /// lines without alphanumerics (significant-line seed rule), so we cover
    /// pure-punctuation, ASCII, and Unicode separately.
    #[test]
    fn hashline_fixed_vectors_are_byte_stable() {
        // Cases captured from the current implementation; if the algorithm
        // changes intentionally these values must be re-snapshotted in the
        // same commit that changes the algorithm.
        let cases: &[(usize, &str, &str)] = &[
            (1, "hello", "TH"),
            (2, "hello", "TH"),
            (1, "world", "SK"),
            (1, "!!!", "TB"),
            (2, "!!!", "QT"),
            (3, "!!!", "MP"),
            (1, "你好", "SS"),
            (1, "fn main() {", "MP"),
            (1, "    println!(\"hello\");", "PR"),
        ];
        // Fail loudly with the actual values so the snapshot is easy to
        // refresh when the algorithm changes deliberately.
        let actual: Vec<(usize, &str, String)> = cases
            .iter()
            .map(|(line, text, _)| (*line, *text, line_hash(*line, text)))
            .collect();
        let expected: Vec<(usize, &str, String)> = cases
            .iter()
            .map(|(line, text, hash)| (*line, *text, hash.to_string()))
            .collect();
        assert_eq!(actual, expected, "hashline byte form drift");
    }

    /// Pure-punctuation lines must be seeded by their line number so that two
    /// identical lines at different positions don’t share an anchor. ASCII
    /// lines with letters/digits must NOT be seeded, so a duplicated line at
    /// any position keeps the same anchor.
    #[test]
    fn hashline_significant_line_seed_rule() {
        // Line number affects pure-punct lines.
        assert_ne!(line_hash(1, "!!!"), line_hash(2, "!!!"));
        assert_ne!(line_hash(2, "!!!"), line_hash(3, "!!!"));
        // Line number does NOT affect lines with alphanumerics.
        assert_eq!(line_hash(1, "hello"), line_hash(2, "hello"));
        assert_eq!(line_hash(7, "fn main() {"), line_hash(99, "fn main() {"));
        // Distinct content must produce distinct anchors most of the time;
        // we don’t require the alphabet to be collision-free, just that
        // these specific samples differ.
        assert_ne!(line_hash(1, "hello"), line_hash(1, "world"));
    }

    #[test]
    fn open_and_read_hashlines() {
        let (dir, engine, tree_id) = seeded_engine("read");
        let scratch = engine.open(VirtualBaseRef::Tree(tree_id)).unwrap();
        let read = engine
            .read(&scratch, "src/lib.rs", ReadMode::Hashlines)
            .unwrap();

        assert_eq!(read.scratch, scratch);
        assert!(read.scratch.as_str().starts_with("scratch:"));
        assert_eq!(read.path, "src/lib.rs");
        assert!(read.file_view_hash.as_str().starts_with("file_view:"));
        assert!(read.content.contains("println!"));
        assert!(read.content.lines().all(|line| line.contains('#')));

        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn write_creates_new_scratch_and_preserves_parent() {
        let (dir, engine, tree_id) = seeded_engine("write");
        let root = engine.open(VirtualBaseRef::Tree(tree_id)).unwrap();
        let write = engine
            .write(&root, "src/main.rs", b"fn main() {}\n")
            .unwrap();
        let read = engine
            .read(&write.scratch, "src/main.rs", ReadMode::Text)
            .unwrap();

        assert_eq!(write.parent, root);
        assert_ne!(write.parent, write.scratch);
        assert_eq!(read.content, "fn main() {}\n");

        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn delete_creates_new_scratch_and_diff_reports_deleted_path() {
        let (dir, engine, tree_id) = seeded_engine("delete");
        let root = engine.open(VirtualBaseRef::Tree(tree_id)).unwrap();
        let delete = engine.delete(&root, "src/lib.rs").unwrap();

        assert_eq!(delete.parent, root);
        assert_ne!(delete.parent, delete.scratch);
        assert_eq!(delete.path, "src/lib.rs");
        let deleted_state = engine.state(&delete.scratch).unwrap();
        assert!(matches!(
            deleted_state.node.op,
            Some(CanonicalScratchOp::Delete { path }) if path == "src/lib.rs"
        ));
        assert!(
            deleted_state
                .tree
                .entries
                .iter()
                .all(|entry| entry.path != "src/lib.rs")
        );
        assert!(matches!(
            engine
                .read(&delete.scratch, "src/lib.rs", ReadMode::Text)
                .unwrap_err(),
            ScratchError::Store(graft_store::StoreError::VirtualPathNotFound(path))
                if path == "src/lib.rs"
        ));
        let diff = engine.diff(&root, &delete.scratch).unwrap();
        assert_eq!(diff.changed_paths, vec!["src/lib.rs".to_string()]);

        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn delete_missing_path_fails_loudly() {
        let (dir, engine, tree_id) = seeded_engine("delete_missing");
        let root = engine.open(VirtualBaseRef::Tree(tree_id)).unwrap();
        let err = engine.delete(&root, "missing.rs").unwrap_err();

        assert!(matches!(
            err,
            ScratchError::Store(graft_store::StoreError::VirtualPathNotFound(path))
                if path == "missing.rs"
        ));

        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn edit_replace_line_checks_hash_and_returns_new_scratch() {
        let (dir, engine, tree_id) = seeded_engine("edit");
        let root = engine.open(VirtualBaseRef::Tree(tree_id)).unwrap();
        let read = engine.read(&root, "src/lib.rs", ReadMode::Text).unwrap();
        let lines = logical_lines(&read.content);
        let hash = line_hash(2, &lines[1]);
        let edit = engine
            .edit(
                &root,
                "src/lib.rs",
                vec![HashlineEdit::ReplaceLine {
                    line: 2,
                    hash,
                    old: "    println!(\"hello\");".to_string(),
                    new: "    println!(\"hi\");".to_string(),
                }],
            )
            .unwrap();

        assert_ne!(edit.parent, edit.scratch);
        assert!(edit.updated_anchors.contains("hi"));
        let read_after = engine
            .read(&edit.scratch, "src/lib.rs", ReadMode::Text)
            .unwrap();
        assert!(read_after.content.contains("hi"));

        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn edit_rejects_stale_anchor_and_display_prefix_payloads() {
        let (dir, engine, tree_id) = seeded_engine("stale");
        let root = engine.open(VirtualBaseRef::Tree(tree_id)).unwrap();
        let stale = engine
            .edit(
                &root,
                "src/lib.rs",
                vec![HashlineEdit::ReplaceLine {
                    line: 2,
                    hash: "ZZ".to_string(),
                    old: "    println!(\"hello\");".to_string(),
                    new: "    println!(\"hi\");".to_string(),
                }],
            )
            .unwrap_err();
        assert!(matches!(stale, ScratchError::StaleAnchor { .. }));

        let read = engine.read(&root, "src/lib.rs", ReadMode::Text).unwrap();
        let lines = logical_lines(&read.content);
        let hash = line_hash(2, &lines[1]);
        let invalid = engine
            .edit(
                &root,
                "src/lib.rs",
                vec![HashlineEdit::ReplaceLine {
                    line: 2,
                    hash,
                    old: "    println!(\"hello\");".to_string(),
                    new: "2#TX:    println!(\"hi\");".to_string(),
                }],
            )
            .unwrap_err();
        assert!(matches!(invalid, ScratchError::InvalidPatch(_)));

        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn candidate_from_scratch_allows_empty_diff() {
        let (dir, engine, tree_id) = seeded_engine("empty_change");
        let root = engine.open(VirtualBaseRef::Tree(tree_id)).unwrap();
        let write = engine
            .write(
                &root,
                "src/lib.rs",
                b"pub fn hello() {\n    println!(\"hello\");\n}\n",
            )
            .unwrap();
        let result = engine
            .candidate_from_scratch(&write.scratch, Vec::new(), "test", None)
            .unwrap();

        assert!(result.changed_paths.is_empty());
        assert!(result.candidate.as_str().starts_with("candidate:"));

        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn capture_tree_creates_root_scratch_for_snapshot_diff() {
        let dir = temp_dir("capture_tree");
        let store = GraftStore::open(&dir);
        store.init().unwrap();
        let readme_hash = store.write_blob(b"# demo\n").unwrap();
        let old_lib_hash = store.write_blob(b"old\n").unwrap();
        let new_lib_hash = store.write_blob(b"new\n").unwrap();
        let main_hash = store.write_blob(b"fn main() {}\n").unwrap();
        let base = TreeSnapshot::new(vec![
            TreeEntry {
                path: "README.md".to_string(),
                hash: readme_hash,
                size: 7,
            },
            TreeEntry {
                path: "src/lib.rs".to_string(),
                hash: old_lib_hash,
                size: 4,
            },
        ]);
        let target = TreeSnapshot::new(vec![
            TreeEntry {
                path: "src/lib.rs".to_string(),
                hash: new_lib_hash,
                size: 4,
            },
            TreeEntry {
                path: "src/main.rs".to_string(),
                hash: main_hash,
                size: 13,
            },
        ]);
        let (base_tree, _) = store.write_tree_snapshot(&base).unwrap();
        let (target_tree, _) = store.write_tree_snapshot(&target).unwrap();
        let engine = ScratchEngine::new(store);

        let capture = engine
            .capture_tree(
                StateId::GraftTree(base_tree.clone()),
                &base_tree,
                &target_tree,
            )
            .unwrap();

        assert!(capture.scratch.as_str().starts_with("scratch:"));
        assert_eq!(capture.base_tree, base_tree);
        assert_eq!(capture.target_tree, target_tree);
        assert_eq!(
            capture.changed_paths,
            vec![
                "README.md".to_string(),
                "src/lib.rs".to_string(),
                "src/main.rs".to_string(),
            ]
        );
        assert_eq!(
            engine
                .read(&capture.scratch, "src/lib.rs", ReadMode::Text)
                .unwrap()
                .content,
            "new\n"
        );

        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn capture_tree_rejects_empty_diff() {
        let (dir, engine, tree_id) = seeded_engine("capture_empty");

        let err = engine
            .capture_tree(StateId::GraftTree(tree_id.clone()), &tree_id, &tree_id)
            .unwrap_err();

        assert!(matches!(err, ScratchError::EmptyChange));

        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn read_after_lost_lease_returns_scratch_lost() {
        let (dir, engine, tree_id) = seeded_engine("scratch_lost");
        let scratch = engine.open(VirtualBaseRef::Tree(tree_id)).unwrap();
        let restarted = ScratchEngine::new(GraftStore::open(&dir));
        let err = restarted
            .read(&scratch, "src/lib.rs", ReadMode::Text)
            .unwrap_err();

        assert!(matches!(err, ScratchError::ScratchLost(_)));

        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn candidate_from_scratch_writes_candidate_change_and_empty_evidence_index() {
        let dir = temp_dir("candidate_from_scratch");
        let store = GraftStore::open(&dir);
        store.init().unwrap();
        let lib_hash = store.write_blob(b"pub fn hello() {}\n").unwrap();
        let readme_hash = store.write_blob(b"# demo\n").unwrap();
        let snapshot = TreeSnapshot::new(vec![
            TreeEntry {
                path: "README.md".to_string(),
                hash: readme_hash,
                size: 7,
            },
            TreeEntry {
                path: "src/lib.rs".to_string(),
                hash: lib_hash,
                size: 18,
            },
        ]);
        let (tree_id, _) = store.write_tree_snapshot(&snapshot).unwrap();
        let engine = ScratchEngine::new(store);
        let root = engine.open(VirtualBaseRef::Tree(tree_id)).unwrap();
        let modified = engine
            .write(
                &root,
                "src/lib.rs",
                b"pub fn hello() { println!(\"hi\"); }\n",
            )
            .unwrap();
        let added = engine
            .write(&modified.scratch, "src/main.rs", b"fn main() {}\n")
            .unwrap();
        let deleted = engine.delete(&added.scratch, "README.md").unwrap();
        let result = engine
            .candidate_from_scratch(
                &deleted.scratch,
                vec![ScopedPropertyRef::new(
                    PropertyScope::Workspace,
                    PropertyRef::new(PropertyId::new("property:review-policy"), "ReviewPolicy"),
                )],
                "test",
                Some("demo".to_string()),
            )
            .unwrap();

        assert_eq!(result.scratch, deleted.scratch);
        assert!(result.candidate.as_str().starts_with("candidate:"));
        assert_eq!(
            result.changed_paths,
            vec![
                "README.md".to_string(),
                "src/lib.rs".to_string(),
                "src/main.rs".to_string(),
            ]
        );

        let candidate = engine
            .store
            .read_candidate(result.candidate.as_str())
            .unwrap();
        let resolved = engine
            .store
            .resolve_application(&candidate.application)
            .unwrap();
        let change = resolved.change;
        assert!(
            change
                .endpoint_diff()
                .iter()
                .any(|file| { file.path == "README.md" && file.kind == FileChangeKind::Deleted })
        );
        assert!(
            change
                .endpoint_diff()
                .iter()
                .any(|file| { file.path == "src/lib.rs" && file.kind == FileChangeKind::Modified })
        );
        assert!(
            change
                .endpoint_diff()
                .iter()
                .any(|file| { file.path == "src/main.rs" && file.kind == FileChangeKind::Added })
        );
        assert_eq!(
            engine
                .store
                .candidate_evidence_index(result.candidate.as_str())
                .unwrap(),
            Vec::<String>::new()
        );

        let _ = std::fs::remove_dir_all(dir);
    }

    /// A `replace_range` edit whose start anchor still matches but whose end
    /// anchor has drifted must surface a stale-anchor error that
    /// includes BOTH endpoints in `fresh_anchors`. This guards against the
    /// previous behavior where only the first-failing endpoint’s context
    /// window was returned, forcing the caller to re-read the entire file
    /// just to find a fresh end anchor.
    #[test]
    fn replace_range_single_end_stale_preserves_other_end() {
        let dir = temp_dir("range_stale");
        let store = GraftStore::open(&dir);
        store.init().unwrap();
        let body = b"alpha\nbeta\ngamma\ndelta\nepsilon\nzeta\neta\n";
        let hash = store.write_blob(body).unwrap();
        let snapshot = TreeSnapshot::new(vec![TreeEntry {
            path: "file.txt".to_string(),
            hash,
            size: body.len() as u64,
        }]);
        let (tree_id, _) = store.write_tree_snapshot(&snapshot).unwrap();
        let engine = ScratchEngine::new(store);
        let root = engine.open(VirtualBaseRef::Tree(tree_id)).unwrap();

        let read = engine.read(&root, "file.txt", ReadMode::Text).unwrap();
        let lines = logical_lines(&read.content);
        let start_hash = line_hash(2, &lines[1]);
        let bogus_end_hash = "ZZ".to_string();
        let err = engine
            .edit(
                &root,
                "file.txt",
                vec![HashlineEdit::ReplaceRange {
                    start_line: 2,
                    start_hash: start_hash.clone(),
                    end_line: 5,
                    end_hash: bogus_end_hash,
                    new_lines: vec!["X".to_string()],
                }],
            )
            .unwrap_err();

        match err {
            ScratchError::StaleAnchor {
                line,
                fresh_anchors,
                ..
            } => {
                // The stale endpoint is line 5.
                assert_eq!(line, 5);
                // Both endpoints must appear, each marked with `>>>` so the
                // caller can re-anchor without losing the other end.
                assert!(
                    fresh_anchors.contains(&format!(">>> 2#{start_hash}:beta")),
                    "start anchor missing or unmarked: {fresh_anchors}"
                );
                let end_actual_hash = line_hash(5, &lines[4]);
                assert!(
                    fresh_anchors.contains(&format!(">>> 5#{end_actual_hash}:epsilon")),
                    "end anchor missing or unmarked: {fresh_anchors}"
                );
            }
            other => panic!("expected StaleAnchor, got {other:?}"),
        }

        let _ = std::fs::remove_dir_all(dir);
    }
}
