use super::*;
use graft_core::{
    AdmissionSummary, PatchId, PatchRelation, PatchRelationKind, PromotionId, PromotionRecord,
    Provenance, RelationId, StateId, materialize_application,
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
    let constraints_roto = fs::read_to_string(dir.join("constraints.roto")).unwrap();
    assert!(constraints_roto.contains("Graft v2 constraint source"));
    assert!(constraints_roto.contains("fn constraint_name(app: Application) -> Constraint"));
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
    let error =
        normalize_relative_path(Path::new("root"), Path::new("root/../escape.txt")).unwrap_err();

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
    fs::create_dir_all(dir.join("constraints")).unwrap();
    fs::write(dir.join("graft.toml"), "schema = 1\n").unwrap();
    fs::write(dir.join("graft.lock"), "version = 1\n").unwrap();
    fs::write(dir.join("constraints.roto"), "fn prop() {}\n").unwrap();
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
    fs::write(dir.join("constraints").join("custom.roto"), "constraint\n").unwrap();

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
            "constraints.roto",
            "constraints/custom.roto",
            "dist/bundle.js",
            "graft.lock",
            "graft.toml",
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
    let dir = std::env::temp_dir().join(format!("graft-store-search-test-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    let store = GraftStore::open(&dir);
    store.init().unwrap();

    let constraint = PlanId::new("plan:observation");
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
        constraint: Constraint::primitive(constraint.clone()),
        provenance: Provenance {
            producer: "test".to_string(),
            message: None,
            created_at: "now".to_string(),
        },
        admission: AdmissionSummary {
            constraint: Constraint::primitive(constraint.clone()),
        },
    };
    store.write_patch(&patch).unwrap();

    assert_eq!(
        store.search_patches_by_plan(&constraint).unwrap(),
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

    let constraint = PlanId::new("plan:indexcopy");
    let evidence = EvidenceRecord::passed("candidate:demo", constraint, "test-verifier").unwrap();
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
    assert_eq!(patch_evidence[0].plan, PlanId::new("plan:indexcopy"));
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

    let candidate_evidence =
        EvidenceRecord::passed("candidate:demo", PlanId::new("plan:candidate"), "test").unwrap();
    let patch_evidence =
        EvidenceRecord::passed("patch:demo", PlanId::new("plan:patch"), "test").unwrap();
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
    let dir = std::env::temp_dir().join(format!("graft-store-record-test-{}", std::process::id()));
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
