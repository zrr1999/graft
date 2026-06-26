mod engine;
mod hashlines;
mod ops;

#[cfg(test)]
use hashlines::{HASHLINE_ALPHABET, line_hash, logical_lines, render_hashlines};

use std::collections::HashMap;
use std::sync::Mutex;

use graft_core::{
    CanonicalScratchOp, Change, Constraint, FileViewHash, FileViewHashSeed, GraftCandidate,
    HashlineEdit, ScratchId, ScratchNode, StateId, TreeEntry, TreeSnapshot, candidate_id,
    file_view_hash, materialize_application, scratch_id,
};
use graft_store::{GraftStore, StoreError, VirtualBaseRef, VirtualFile};

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
pub struct ScratchBaseMetadata {
    pub base_state: StateId,
    pub base_tree: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ScratchOpen {
    pub scratch: ScratchId,
    pub base_state: StateId,
    pub base_tree: String,
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
    pub base_state: StateId,
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
pub(crate) struct ScratchState {
    pub(crate) node: ScratchNode,
    pub(crate) base_tree: TreeSnapshot,
    pub(crate) tree: TreeSnapshot,
}

#[derive(Debug)]
pub struct ScratchEngine {
    store: GraftStore,
    states: Mutex<HashMap<ScratchId, ScratchState>>,
    leases: Mutex<HashMap<String, ScratchId>>,
}

#[cfg(test)]
mod tests;
