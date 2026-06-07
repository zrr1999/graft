use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

use crate::{
    ActionId, ApplicationId, ChangeId, ChangeSummary, FileChange, FileChangeKind, Result, StateId,
    TreeSnapshot, stable_typed_id,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FileMode {
    Regular,
    Executable,
    Symlink,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Action {
    CreateFile {
        path: String,
        blob: String,
        mode: FileMode,
        if_absent: bool,
    },
    DeleteFile {
        path: String,
        expect_blob: Option<String>,
    },
    ReplaceFile {
        path: String,
        expect_blob: Option<String>,
        blob: String,
        mode: FileMode,
    },
    Sequence {
        steps: Vec<Action>,
    },
}

impl Action {
    pub fn from_change_ops(ops: &[ChangeOp]) -> Self {
        Self::Sequence {
            steps: ops
                .iter()
                .map(|op| match op {
                    ChangeOp::CreateFile { path, blob, mode } => Self::CreateFile {
                        path: path.clone(),
                        blob: blob.clone(),
                        mode: *mode,
                        if_absent: true,
                    },
                    ChangeOp::DeleteFile { path, blob, .. } => Self::DeleteFile {
                        path: path.clone(),
                        expect_blob: Some(blob.clone()),
                    },
                    ChangeOp::ReplaceFile {
                        path,
                        before,
                        after,
                        mode_after,
                        ..
                    } => Self::ReplaceFile {
                        path: path.clone(),
                        expect_blob: Some(before.clone()),
                        blob: after.clone(),
                        mode: *mode_after,
                    },
                    ChangeOp::Rename {
                        from,
                        to,
                        blob,
                        mode,
                    } => Self::Sequence {
                        steps: vec![
                            Self::CreateFile {
                                path: to.clone(),
                                blob: blob.clone(),
                                mode: *mode,
                                if_absent: true,
                            },
                            Self::DeleteFile {
                                path: from.clone(),
                                expect_blob: Some(blob.clone()),
                            },
                        ],
                    },
                    ChangeOp::Chmod {
                        path,
                        blob,
                        mode_after,
                        ..
                    } => Self::ReplaceFile {
                        path: path.clone(),
                        expect_blob: Some(blob.clone()),
                        blob: blob.clone(),
                        mode: *mode_after,
                    },
                })
                .collect(),
        }
    }
}

pub fn action_id(action: &Action) -> Result<ActionId> {
    Ok(ActionId::new(stable_typed_id("action", action)?))
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ApplicabilityStep {
    CreateFile {
        path: String,
        observed_missing: bool,
    },
    DeleteFile {
        path: String,
        matched_blob: String,
    },
    ReplaceFile {
        path: String,
        before_blob: String,
        after_blob: String,
    },
    Sequence {
        children: Vec<ApplicabilityStep>,
    },
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ApplicabilityProof {
    pub action: ActionId,
    pub base_state: StateId,
    pub steps: Vec<ApplicabilityStep>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ChangeOp {
    CreateFile {
        path: String,
        blob: String,
        mode: FileMode,
    },
    DeleteFile {
        path: String,
        blob: String,
        mode: FileMode,
    },
    ReplaceFile {
        path: String,
        before: String,
        after: String,
        mode_before: FileMode,
        mode_after: FileMode,
    },
    Rename {
        from: String,
        to: String,
        blob: String,
        mode: FileMode,
    },
    Chmod {
        path: String,
        blob: String,
        mode_before: FileMode,
        mode_after: FileMode,
    },
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Change {
    pub base_state: StateId,
    pub target_state: StateId,
    pub ops: Vec<ChangeOp>,
    #[serde(default)]
    pub capture: bool,
}

impl Change {
    pub fn from_snapshots(
        base_state: StateId,
        base: Option<&TreeSnapshot>,
        target_state: StateId,
        target: &TreeSnapshot,
    ) -> Self {
        let capture = base.is_none();
        let files = endpoint_diff_from_snapshots(base, target);
        let mut ops = files
            .iter()
            .filter_map(file_change_to_op)
            .collect::<Vec<_>>();
        sort_ops(&mut ops);
        Self {
            base_state,
            target_state,
            ops,
            capture,
        }
    }

    pub fn id(&self) -> Result<ChangeId> {
        Ok(ChangeId::new(stable_typed_id("change", self)?))
    }

    pub fn endpoint_diff(&self) -> Vec<FileChange> {
        self.ops
            .iter()
            .filter_map(|op| op_to_file_change(op, self.capture))
            .collect()
    }

    pub fn changed_paths(&self) -> Vec<String> {
        let mut paths = self
            .endpoint_diff()
            .into_iter()
            .map(|file| file.path)
            .collect::<Vec<_>>();
        paths.sort();
        paths
    }

    pub fn compose(first: &Self, second: &Self) -> Self {
        #[derive(Default)]
        struct Endpoints {
            base_hash: Option<String>,
            target_hash: Option<String>,
            base_size: Option<u64>,
            target_size: Option<u64>,
            has_base: bool,
        }

        let mut endpoints = BTreeMap::<String, Endpoints>::new();
        for file in first.endpoint_diff() {
            let entry = endpoints.entry(file.path.clone()).or_default();
            entry.base_hash = file.base_hash.clone();
            entry.base_size = file.base_size;
            entry.target_hash = file.target_hash.clone();
            entry.target_size = file.target_size;
            entry.has_base = true;
        }
        for file in second.endpoint_diff() {
            let entry = endpoints.entry(file.path.clone()).or_default();
            if !entry.has_base {
                entry.base_hash = file.base_hash.clone();
                entry.base_size = file.base_size;
                entry.has_base = true;
            }
            entry.target_hash = file.target_hash.clone();
            entry.target_size = file.target_size;
        }

        let files = endpoints
            .into_iter()
            .filter_map(|(path, entry)| {
                let kind = file_change_kind(&entry.base_hash, &entry.target_hash)?;
                Some(FileChange {
                    path,
                    kind,
                    base_hash: entry.base_hash,
                    target_hash: entry.target_hash,
                    base_size: entry.base_size,
                    target_size: entry.target_size,
                })
            })
            .collect::<Vec<_>>();
        let mut ops = files
            .iter()
            .filter_map(file_change_to_op)
            .collect::<Vec<_>>();
        sort_ops(&mut ops);
        Self {
            base_state: first.base_state.clone(),
            target_state: second.target_state.clone(),
            ops,
            capture: false,
        }
    }

    pub fn migrated(&self, base_state: StateId) -> Self {
        Self {
            base_state,
            target_state: self.target_state.clone(),
            ops: self.ops.clone(),
            capture: false,
        }
    }

    pub fn reversed(&self) -> Self {
        let mut ops = self.ops.iter().map(reverse_op).collect::<Vec<_>>();
        sort_ops(&mut ops);
        Self {
            base_state: self.target_state.clone(),
            target_state: self.base_state.clone(),
            ops,
            capture: false,
        }
    }

    pub fn summary(&self) -> ChangeSummary {
        let mut summary = ChangeSummary::default();
        for file in self.endpoint_diff() {
            summary.files += 1;
            match file.kind {
                FileChangeKind::Added => summary.added += 1,
                FileChangeKind::Modified => summary.modified += 1,
                FileChangeKind::Deleted => summary.deleted += 1,
                FileChangeKind::Unchanged => summary.unchanged += 1,
                FileChangeKind::Captured => summary.captured += 1,
            }
            summary.target_bytes += file.target_size.unwrap_or(0);
        }
        summary
    }

    pub fn proof_steps(&self) -> Vec<ApplicabilityStep> {
        self.ops
            .iter()
            .map(|op| match op {
                ChangeOp::CreateFile { path, .. } => ApplicabilityStep::CreateFile {
                    path: path.clone(),
                    observed_missing: true,
                },
                ChangeOp::DeleteFile { path, blob, .. } => ApplicabilityStep::DeleteFile {
                    path: path.clone(),
                    matched_blob: blob.clone(),
                },
                ChangeOp::ReplaceFile {
                    path,
                    before,
                    after,
                    ..
                } => ApplicabilityStep::ReplaceFile {
                    path: path.clone(),
                    before_blob: before.clone(),
                    after_blob: after.clone(),
                },
                ChangeOp::Rename { from, blob, .. } => ApplicabilityStep::Sequence {
                    children: vec![
                        ApplicabilityStep::CreateFile {
                            path: from.clone(),
                            observed_missing: true,
                        },
                        ApplicabilityStep::DeleteFile {
                            path: from.clone(),
                            matched_blob: blob.clone(),
                        },
                    ],
                },
                ChangeOp::Chmod { path, blob, .. } => ApplicabilityStep::ReplaceFile {
                    path: path.clone(),
                    before_blob: blob.clone(),
                    after_blob: blob.clone(),
                },
            })
            .collect()
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ApplicationRecord {
    pub action: ActionId,
    pub base_state: StateId,
    pub applicability_proof: ApplicabilityProof,
    pub target_state: StateId,
    pub change: ChangeId,
    pub lowering_version: u32,
}

impl ApplicationRecord {
    pub const LOWERING_VERSION: u32 = 1;

    pub fn id(&self) -> Result<ApplicationId> {
        application_id(self)
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(
    tag = "kind",
    content = "value",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum ApplicationRef {
    Stored(ApplicationId),
}

pub fn application_id(record: &ApplicationRecord) -> Result<ApplicationId> {
    Ok(ApplicationId::new(stable_typed_id("application", record)?))
}

pub struct MaterializedApplication {
    pub action: Action,
    pub action_id: ActionId,
    pub proof: ApplicabilityProof,
    pub change: Change,
    pub record: ApplicationRecord,
}

pub fn application_from_change(change: &Change) -> Result<MaterializedApplication> {
    let action = Action::from_change_ops(&change.ops);
    let action_id = action_id(&action)?;
    let proof = ApplicabilityProof {
        action: action_id.clone(),
        base_state: change.base_state.clone(),
        steps: change.proof_steps(),
    };
    let change_id = change.id()?;
    let record = ApplicationRecord {
        action: action_id.clone(),
        base_state: change.base_state.clone(),
        applicability_proof: proof.clone(),
        target_state: change.target_state.clone(),
        change: change_id,
        lowering_version: ApplicationRecord::LOWERING_VERSION,
    };
    Ok(MaterializedApplication {
        action,
        action_id,
        proof,
        change: change.clone(),
        record,
    })
}

pub fn materialize_application(
    base_state: StateId,
    base: Option<&TreeSnapshot>,
    target_state: StateId,
    target: &TreeSnapshot,
) -> Result<MaterializedApplication> {
    let change = Change::from_snapshots(base_state.clone(), base, target_state.clone(), target);
    let action = Action::from_change_ops(&change.ops);
    let action_id = action_id(&action)?;
    let proof = ApplicabilityProof {
        action: action_id.clone(),
        base_state: base_state.clone(),
        steps: change.proof_steps(),
    };
    let change_id = change.id()?;
    let record = ApplicationRecord {
        action: action_id.clone(),
        base_state,
        applicability_proof: proof.clone(),
        target_state,
        change: change_id,
        lowering_version: ApplicationRecord::LOWERING_VERSION,
    };
    Ok(MaterializedApplication {
        action,
        action_id,
        proof,
        change,
        record,
    })
}

fn endpoint_diff_from_snapshots(
    base: Option<&TreeSnapshot>,
    target: &TreeSnapshot,
) -> Vec<FileChange> {
    let mut files = Vec::new();
    let target_entries = target
        .entries
        .iter()
        .map(|entry| (entry.path.as_str(), entry))
        .collect::<BTreeMap<_, _>>();

    if let Some(base) = base {
        let base_entries = base
            .entries
            .iter()
            .map(|entry| (entry.path.as_str(), entry))
            .collect::<BTreeMap<_, _>>();
        for (path, base_entry) in &base_entries {
            match target_entries.get(*path) {
                Some(target_entry) if target_entry.hash == base_entry.hash => {}
                Some(target_entry) => {
                    files.push(FileChange {
                        path: (*path).to_string(),
                        kind: FileChangeKind::Modified,
                        base_hash: Some(base_entry.hash.clone()),
                        target_hash: Some(target_entry.hash.clone()),
                        base_size: Some(base_entry.size),
                        target_size: Some(target_entry.size),
                    });
                }
                None => {
                    files.push(FileChange {
                        path: (*path).to_string(),
                        kind: FileChangeKind::Deleted,
                        base_hash: Some(base_entry.hash.clone()),
                        target_hash: None,
                        base_size: Some(base_entry.size),
                        target_size: None,
                    });
                }
            }
        }
        for (path, target_entry) in target_entries {
            if !base_entries.contains_key(path) {
                files.push(FileChange {
                    path: path.to_string(),
                    kind: FileChangeKind::Added,
                    base_hash: None,
                    target_hash: Some(target_entry.hash.clone()),
                    base_size: None,
                    target_size: Some(target_entry.size),
                });
            }
        }
    } else {
        files.extend(target.entries.iter().map(|entry| FileChange {
            path: entry.path.clone(),
            kind: FileChangeKind::Captured,
            base_hash: None,
            target_hash: Some(entry.hash.clone()),
            base_size: None,
            target_size: Some(entry.size),
        }));
    }

    files.sort_by(|left, right| left.path.cmp(&right.path));
    files
}

fn file_change_to_op(file: &FileChange) -> Option<ChangeOp> {
    match file.kind {
        FileChangeKind::Added | FileChangeKind::Captured => {
            let blob = file.target_hash.clone()?;
            Some(ChangeOp::CreateFile {
                path: file.path.clone(),
                blob,
                mode: FileMode::Regular,
            })
        }
        FileChangeKind::Deleted => {
            let blob = file.base_hash.clone()?;
            Some(ChangeOp::DeleteFile {
                path: file.path.clone(),
                blob,
                mode: FileMode::Regular,
            })
        }
        FileChangeKind::Modified => {
            let before = file.base_hash.clone()?;
            let after = file.target_hash.clone()?;
            Some(ChangeOp::ReplaceFile {
                path: file.path.clone(),
                before,
                after,
                mode_before: FileMode::Regular,
                mode_after: FileMode::Regular,
            })
        }
        FileChangeKind::Unchanged => None,
    }
}

fn op_to_file_change(op: &ChangeOp, capture: bool) -> Option<FileChange> {
    match op {
        ChangeOp::CreateFile { path, blob, .. } => Some(FileChange {
            path: path.clone(),
            kind: if capture {
                FileChangeKind::Captured
            } else {
                FileChangeKind::Added
            },
            base_hash: None,
            target_hash: Some(blob.clone()),
            base_size: None,
            target_size: None,
        }),
        ChangeOp::DeleteFile { path, blob, .. } => Some(FileChange {
            path: path.clone(),
            kind: FileChangeKind::Deleted,
            base_hash: Some(blob.clone()),
            target_hash: None,
            base_size: None,
            target_size: None,
        }),
        ChangeOp::ReplaceFile {
            path,
            before,
            after,
            ..
        } => Some(FileChange {
            path: path.clone(),
            kind: FileChangeKind::Modified,
            base_hash: Some(before.clone()),
            target_hash: Some(after.clone()),
            base_size: None,
            target_size: None,
        }),
        ChangeOp::Rename { from, to, blob, .. } => Some(FileChange {
            path: format!("{from}->{to}"),
            kind: FileChangeKind::Modified,
            base_hash: Some(blob.clone()),
            target_hash: Some(blob.clone()),
            base_size: None,
            target_size: None,
        }),
        ChangeOp::Chmod { path, blob, .. } => Some(FileChange {
            path: path.clone(),
            kind: FileChangeKind::Modified,
            base_hash: Some(blob.clone()),
            target_hash: Some(blob.clone()),
            base_size: None,
            target_size: None,
        }),
    }
}

fn reverse_op(op: &ChangeOp) -> ChangeOp {
    match op {
        ChangeOp::CreateFile { path, blob, mode } => ChangeOp::DeleteFile {
            path: path.clone(),
            blob: blob.clone(),
            mode: *mode,
        },
        ChangeOp::DeleteFile { path, blob, mode } => ChangeOp::CreateFile {
            path: path.clone(),
            blob: blob.clone(),
            mode: *mode,
        },
        ChangeOp::ReplaceFile {
            path,
            before,
            after,
            mode_before,
            mode_after,
        } => ChangeOp::ReplaceFile {
            path: path.clone(),
            before: after.clone(),
            after: before.clone(),
            mode_before: *mode_after,
            mode_after: *mode_before,
        },
        ChangeOp::Rename {
            from,
            to,
            blob,
            mode,
        } => ChangeOp::Rename {
            from: to.clone(),
            to: from.clone(),
            blob: blob.clone(),
            mode: *mode,
        },
        ChangeOp::Chmod {
            path,
            blob,
            mode_before,
            mode_after,
        } => ChangeOp::Chmod {
            path: path.clone(),
            blob: blob.clone(),
            mode_before: *mode_after,
            mode_after: *mode_before,
        },
    }
}

fn sort_ops(ops: &mut [ChangeOp]) {
    ops.sort_by_key(op_sort_key);
}

fn op_sort_key(op: &ChangeOp) -> (u8, String) {
    match op {
        ChangeOp::Chmod { path, .. } => (0, path.clone()),
        ChangeOp::CreateFile { path, .. } => (1, path.clone()),
        ChangeOp::DeleteFile { path, .. } => (2, path.clone()),
        ChangeOp::Rename { from, .. } => (3, from.clone()),
        ChangeOp::ReplaceFile { path, .. } => (4, path.clone()),
    }
}

fn file_change_kind(
    base_hash: &Option<String>,
    target_hash: &Option<String>,
) -> Option<FileChangeKind> {
    match (base_hash, target_hash) {
        (None, None) => None,
        (None, Some(_)) => Some(FileChangeKind::Added),
        (Some(_), None) => Some(FileChangeKind::Deleted),
        (Some(base), Some(target)) if base == target => None,
        (Some(_), Some(_)) => Some(FileChangeKind::Modified),
    }
}
