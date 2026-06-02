use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::PathBuf;

use anyhow::{Context, Result};
use graft_core::{
    ChangeRef, ChangeSet, Evaluator, EvidenceRecord, EvidenceResult, FileChange, FileChangeKind,
    GraftCandidate, PatchRecord, PropertyDef, PropertyRef, StateId, TreeEntry, TreeSnapshot,
};
use graft_store::GraftStore;
use graft_validate::{ValidationEngine, ValidationSubject};
use serde::{Deserialize, Serialize};

use crate::config::{GraftConfig, load_graft_config};
use crate::repo::materialized_snapshot_for_state;
use crate::requirements::validation_properties_with_base;

struct ValidationTarget {
    subject: ValidationSubject,
    snapshot: Option<TreeSnapshot>,
}

#[derive(Debug, Eq, PartialEq, Deserialize, Serialize)]
struct ValidationWorktreeManifest {
    schema: u32,
    subject: String,
    property: String,
    snapshot: String,
    verifier: String,
}

pub(crate) fn validate_candidate(
    store: &GraftStore,
    candidate: &GraftCandidate,
    expected: &[String],
) -> Result<Vec<EvidenceRecord>> {
    let config = load_graft_config(store)?;
    let target =
        validation_target_for_change(store, &config, candidate.id.as_str(), &candidate.change)?;
    let properties = validation_properties_with_base(&config, expected, &candidate.expected)?;
    let mut records = Vec::new();
    let mut memo = BTreeMap::new();
    for property in properties {
        let mut property_records =
            validate_property(store, &config, &target, &property, &mut memo)?;
        for evidence in &property_records {
            store.write_evidence(evidence)?;
            store.append_candidate_evidence_index(candidate.id.as_str(), evidence.id.as_str())?;
        }
        records.append(&mut property_records);
    }
    Ok(records)
}

pub(crate) fn validate_patch(
    store: &GraftStore,
    patch: &PatchRecord,
    expected: &[String],
) -> Result<Vec<EvidenceRecord>> {
    let config = load_graft_config(store)?;
    let target = validation_target_for_change(store, &config, patch.id.as_str(), &patch.change)?;
    let properties = validation_properties_with_base(&config, expected, &patch.properties)?;
    let mut records = Vec::new();
    let mut memo = BTreeMap::new();
    for property in properties {
        let mut property_records =
            validate_property(store, &config, &target, &property, &mut memo)?;
        for evidence in &property_records {
            store.write_evidence(evidence)?;
            store.append_patch_evidence_index(patch.id.as_str(), evidence.id.as_str())?;
        }
        records.append(&mut property_records);
    }
    Ok(records)
}

fn validate_property(
    store: &GraftStore,
    config: &GraftConfig,
    target: &ValidationTarget,
    property: &PropertyRef,
    memo: &mut BTreeMap<String, Vec<EvidenceRecord>>,
) -> Result<Vec<EvidenceRecord>> {
    if let Some(records) = memo.get(property.id.as_str()) {
        return Ok(records.clone());
    }
    let property_config = config.properties.get(&property.name).with_context(|| {
        format!(
            "property {} is not configured in properties/*.toml",
            property.name
        )
    })?;
    let records = vec![verify_property(store, target, property, property_config)?];
    memo.insert(property.id.as_str().to_string(), records.clone());
    Ok(records)
}

fn verify_property(
    store: &GraftStore,
    target: &ValidationTarget,
    property: &PropertyRef,
    def: &PropertyDef,
) -> Result<EvidenceRecord> {
    let verifier_id = verifier_id_for_property(def)?;
    if matches!(
        &def.evaluator,
        Evaluator::Command { .. } | Evaluator::Pair { .. }
    ) && target.snapshot.is_none()
    {
        return Ok(EvidenceRecord::unknown(
            target.subject.id.clone(),
            property.id.clone(),
            verifier_id,
            "no materializable target snapshot was available for isolated validation",
        )?);
    }
    let (subject, cwd) = if let Some(snapshot) = &target.snapshot {
        let worktree =
            prepare_validation_worktree(store, &target.subject.id, &property.name, def, snapshot)?;
        (
            target
                .subject
                .clone()
                .with_validation_worktree(worktree.clone()),
            worktree,
        )
    } else {
        (
            target.subject.clone(),
            store.paths().workspace().to_path_buf(),
        )
    };
    Ok(ValidationEngine::new(cwd).validate(&subject, def)?)
}

fn verifier_id_for_property(def: &PropertyDef) -> Result<String> {
    let kind = match &def.evaluator {
        Evaluator::Builtin { .. } => "builtin",
        Evaluator::Command { .. } => "command",
        Evaluator::Pair { .. } => "pair",
    };
    Ok(format!("{kind}:{}@{}", def.name, def.property_id()?))
}

pub(crate) fn evidence_for_current_verifiers(
    _config: &GraftConfig,
    required: &[PropertyRef],
    evidence: &[EvidenceRecord],
) -> Result<Vec<EvidenceRecord>> {
    Ok(evidence
        .iter()
        .filter(|record| {
            required
                .iter()
                .any(|property| property.id == record.property)
        })
        .cloned()
        .collect())
}

fn validation_target_for_change(
    store: &GraftStore,
    config: &GraftConfig,
    id: &str,
    change: &ChangeRef,
) -> Result<ValidationTarget> {
    match change {
        ChangeRef::Stored(change_id) => {
            let change = store.read_change(change_id.as_str())?;
            let changed_paths = change
                .files
                .iter()
                .filter(|file| !matches!(file.kind, FileChangeKind::Unchanged))
                .map(|file| file.path.clone())
                .collect();
            let target_snapshot =
                materialized_snapshot_for_state(store, config, &change.target_state)?;
            let patch_validity =
                patch_validity_for_change(store, config, &change, &target_snapshot);
            Ok(ValidationTarget {
                subject: ValidationSubject::with_change(id.to_string(), changed_paths)
                    .with_patch_validity(patch_validity),
                snapshot: Some(target_snapshot),
            })
        }
        ChangeRef::InlineSummary(_) => Ok(ValidationTarget {
            subject: ValidationSubject::new(id.to_string()),
            snapshot: None,
        }),
    }
}

fn patch_validity_for_change(
    store: &GraftStore,
    config: &GraftConfig,
    change: &ChangeSet,
    target_snapshot: &TreeSnapshot,
) -> EvidenceResult {
    let base_snapshot = match materialized_snapshot_for_state(store, config, &change.base_state) {
        Ok(snapshot) => snapshot,
        Err(error) => {
            return EvidenceResult::Unknown {
                reason: graft_explain::diagnostics::v003_base_unmaterializable(&error.to_string())
                    .format_reason(),
            };
        }
    };
    match validate_change_replays_to_target(change, &base_snapshot, target_snapshot) {
        Ok(()) => EvidenceResult::Passed,
        Err(reason) => EvidenceResult::Failed { reason },
    }
}

fn validate_change_replays_to_target(
    change: &ChangeSet,
    base_snapshot: &TreeSnapshot,
    target_snapshot: &TreeSnapshot,
) -> std::result::Result<(), String> {
    ensure_snapshot_matches_state("base", &change.base_state, base_snapshot)?;
    ensure_snapshot_matches_state("target", &change.target_state, target_snapshot)?;

    let mut applied = snapshot_entries(base_snapshot);
    let mut seen = BTreeSet::new();
    for file in &change.files {
        if !seen.insert(file.path.clone()) {
            return Err(format!("duplicate change entry for path {}", file.path));
        }
        match file.kind {
            FileChangeKind::Added | FileChangeKind::Captured => {
                if applied.contains_key(&file.path) {
                    return Err(format!("path {} already exists in base", file.path));
                }
                let entry = target_entry_from_change(file)?;
                applied.insert(file.path.clone(), entry);
            }
            FileChangeKind::Modified => {
                let Some(base_entry) = applied.get(&file.path) else {
                    return Err(format!("path {} is missing from base", file.path));
                };
                ensure_change_matches_base(file, base_entry)?;
                let entry = target_entry_from_change(file)?;
                applied.insert(file.path.clone(), entry);
            }
            FileChangeKind::Deleted => {
                let Some(base_entry) = applied.get(&file.path) else {
                    return Err(format!("path {} is missing from base", file.path));
                };
                ensure_change_matches_base(file, base_entry)?;
                if file.target_hash.is_some() || file.target_size.is_some() {
                    return Err(format!("deleted path {} has target content", file.path));
                }
                applied.remove(&file.path);
            }
            FileChangeKind::Unchanged => {
                let Some(base_entry) = applied.get(&file.path) else {
                    return Err(format!("unchanged path {} is missing from base", file.path));
                };
                ensure_change_matches_base(file, base_entry)?;
                let entry = target_entry_from_change(file)?;
                if entry != *base_entry {
                    return Err(format!(
                        "unchanged path {} changes content in target",
                        file.path
                    ));
                }
            }
        }
    }

    let expected = snapshot_entries(target_snapshot);
    if applied == expected {
        Ok(())
    } else {
        Err(snapshot_mismatch_reason(&applied, &expected))
    }
}

fn ensure_snapshot_matches_state(
    label: &str,
    state: &StateId,
    snapshot: &TreeSnapshot,
) -> std::result::Result<(), String> {
    if let StateId::GraftTree(expected) = state {
        let actual = snapshot
            .id()
            .map_err(|error| format!("{label} snapshot id could not be computed: {error}"))?;
        if actual != *expected {
            return Err(format!(
                "{label} snapshot id mismatch: state declares {expected}, snapshot is {actual}"
            ));
        }
    }
    Ok(())
}

fn snapshot_entries(snapshot: &TreeSnapshot) -> BTreeMap<String, TreeEntry> {
    snapshot
        .entries
        .iter()
        .map(|entry| (entry.path.clone(), entry.clone()))
        .collect()
}

fn target_entry_from_change(file: &FileChange) -> std::result::Result<TreeEntry, String> {
    let Some(hash) = &file.target_hash else {
        return Err(format!("path {} has no target hash", file.path));
    };
    let Some(size) = file.target_size else {
        return Err(format!("path {} has no target size", file.path));
    };
    Ok(TreeEntry {
        path: file.path.clone(),
        hash: hash.clone(),
        size,
    })
}

fn ensure_change_matches_base(
    file: &FileChange,
    base_entry: &TreeEntry,
) -> std::result::Result<(), String> {
    if file.base_hash.as_deref() != Some(base_entry.hash.as_str())
        || file.base_size != Some(base_entry.size)
    {
        return Err(format!(
            "path {} does not match declared base content",
            file.path
        ));
    }
    Ok(())
}

fn snapshot_mismatch_reason(
    applied: &BTreeMap<String, TreeEntry>,
    expected: &BTreeMap<String, TreeEntry>,
) -> String {
    let paths = applied
        .keys()
        .chain(expected.keys())
        .cloned()
        .collect::<BTreeSet<_>>();
    for path in paths {
        match (applied.get(&path), expected.get(&path)) {
            (None, Some(_)) => return format!("target contains path {path} absent after replay"),
            (Some(_), None) => return format!("replay produced path {path} absent from target"),
            (Some(left), Some(right)) if left != right => {
                return format!("replay produced different content for path {path}");
            }
            _ => {}
        }
    }
    "replayed patch did not reconstruct target".to_string()
}

fn prepare_validation_worktree(
    store: &GraftStore,
    subject_id: &str,
    property: &str,
    def: &PropertyDef,
    snapshot: &TreeSnapshot,
) -> Result<PathBuf> {
    let snapshot_id = snapshot.id()?;
    let verifier_fingerprint = def.property_id()?.to_string();
    let path = store
        .paths()
        .cache_worktrees()
        .join(safe_path_component(subject_id))
        .join(safe_path_component(property))
        .join(safe_path_component(&snapshot_id));
    let manifest = ValidationWorktreeManifest {
        schema: 1,
        subject: subject_id.to_string(),
        property: property.to_string(),
        snapshot: snapshot_id,
        verifier: verifier_fingerprint,
    };
    let manifest_path = path.join(".graft-validation.toml");
    if manifest_path.exists() {
        let text = fs::read_to_string(&manifest_path)
            .with_context(|| format!("read {}", manifest_path.display()))?;
        if toml::from_str::<ValidationWorktreeManifest>(&text)
            .map(|existing| existing == manifest)
            .unwrap_or(false)
        {
            return Ok(path);
        }
    }
    store.materialize_tree_snapshot(snapshot, &path)?;
    fs::write(
        &manifest_path,
        toml::to_string_pretty(&manifest)
            .with_context(|| format!("serialize {}", manifest_path.display()))?,
    )
    .with_context(|| format!("write {}", manifest_path.display()))?;
    Ok(path)
}

fn safe_path_component(value: &str) -> String {
    let mut component = value
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || matches!(ch, '.' | '-' | '_') {
                ch
            } else {
                '_'
            }
        })
        .collect::<String>();
    if component.is_empty() {
        component.push_str("value");
    }
    component.truncate(80);
    component
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::fs;
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

    use graft_core::{ChangeSet, Evaluator, Judge, Query, StateId, TreeEntry, TreeSnapshot};
    use graft_store::GraftStore;

    use super::{prepare_validation_worktree, validate_change_replays_to_target};

    #[test]
    fn valid_patch_replays_change_to_target() {
        let base = TreeSnapshot::new(vec![TreeEntry {
            path: "src/lib.rs".to_string(),
            hash: "old".to_string(),
            size: 3,
        }]);
        let target = TreeSnapshot::new(vec![TreeEntry {
            path: "src/lib.rs".to_string(),
            hash: "new".to_string(),
            size: 3,
        }]);
        let change = ChangeSet::from_snapshots(
            StateId::GraftTree(base.id().unwrap()),
            Some(&base),
            StateId::GraftTree(target.id().unwrap()),
            &target,
        );

        assert!(validate_change_replays_to_target(&change, &base, &target).is_ok());
    }

    #[test]
    fn valid_patch_fails_when_base_content_does_not_match() {
        let base = TreeSnapshot::new(vec![TreeEntry {
            path: "src/lib.rs".to_string(),
            hash: "old".to_string(),
            size: 3,
        }]);
        let target = TreeSnapshot::new(vec![TreeEntry {
            path: "src/lib.rs".to_string(),
            hash: "new".to_string(),
            size: 3,
        }]);
        let mut change = ChangeSet::from_snapshots(
            StateId::GraftTree(base.id().unwrap()),
            Some(&base),
            StateId::GraftTree(target.id().unwrap()),
            &target,
        );
        change.files[0].base_hash = Some("other".to_string());

        let reason = validate_change_replays_to_target(&change, &base, &target).unwrap_err();

        assert!(reason.contains("does not match declared base content"));
    }

    #[test]
    fn prepares_isolated_validation_worktree_from_snapshot() {
        let dir = test_workspace("graft-cli-isolated-validation-test");
        let store = GraftStore::open(&dir);
        store.init_storage().unwrap();
        let hash = store.write_blob(b"target\n").unwrap();
        let snapshot = TreeSnapshot::new(vec![TreeEntry {
            path: "src/lib.rs".to_string(),
            hash,
            size: 7,
        }]);

        let def = command_property_def("TestsPass", "true");
        let worktree =
            prepare_validation_worktree(&store, "candidate:demo", "TestsPass", &def, &snapshot)
                .unwrap();

        assert_eq!(
            fs::read_to_string(worktree.join("src").join("lib.rs")).unwrap(),
            "target\n"
        );
        assert!(worktree.starts_with(store.paths().cache_worktrees()));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn reuses_matching_validation_worktree() {
        let dir = test_workspace("graft-cli-validation-reuse-test");
        let store = GraftStore::open(&dir);
        store.init_storage().unwrap();
        let hash = store.write_blob(b"target\n").unwrap();
        let snapshot = TreeSnapshot::new(vec![TreeEntry {
            path: "src/lib.rs".to_string(),
            hash,
            size: 7,
        }]);

        let def = command_property_def("TestsPass", "true");
        let worktree =
            prepare_validation_worktree(&store, "candidate:demo", "TestsPass", &def, &snapshot)
                .unwrap();
        fs::write(worktree.join("reuse.marker"), "kept").unwrap();
        let reused =
            prepare_validation_worktree(&store, "candidate:demo", "TestsPass", &def, &snapshot)
                .unwrap();

        assert_eq!(reused, worktree);
        assert_eq!(
            fs::read_to_string(reused.join("reuse.marker")).unwrap(),
            "kept"
        );
        assert!(reused.join(".graft-validation.toml").exists());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn rebuilds_validation_worktree_when_verifier_changes() {
        let dir = test_workspace("graft-cli-validation-rebuild-test");
        let store = GraftStore::open(&dir);
        store.init_storage().unwrap();
        let hash = store.write_blob(b"target\n").unwrap();
        let snapshot = TreeSnapshot::new(vec![TreeEntry {
            path: "src/lib.rs".to_string(),
            hash,
            size: 7,
        }]);

        let def = command_property_def("TestsPass", "true");
        let worktree =
            prepare_validation_worktree(&store, "candidate:demo", "TestsPass", &def, &snapshot)
                .unwrap();
        fs::write(worktree.join("stale.marker"), "old verifier output").unwrap();
        let changed_def = command_property_def("TestsPass", "false");

        let rebuilt = prepare_validation_worktree(
            &store,
            "candidate:demo",
            "TestsPass",
            &changed_def,
            &snapshot,
        )
        .unwrap();

        assert_eq!(rebuilt, worktree);
        assert!(!rebuilt.join("stale.marker").exists());
        assert_eq!(
            fs::read_to_string(rebuilt.join("src").join("lib.rs")).unwrap(),
            "target\n"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    fn command_property_def(name: &str, command: &str) -> graft_core::PropertyDef {
        graft_core::PropertyDef {
            name: name.to_string(),
            query: Query::Change,
            evaluator: Evaluator::Command {
                command: command.to_string(),
                args: Vec::new(),
                env: BTreeMap::new(),
                setup: Vec::new(),
                pre: Vec::new(),
                teardown: Vec::new(),
                timeout_secs: None,
            },
            judge: Judge::ExitCodeZero,
        }
    }

    fn test_workspace(name: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!("{name}-{}-{nanos}", std::process::id()))
    }
}
