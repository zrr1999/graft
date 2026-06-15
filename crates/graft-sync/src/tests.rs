use super::*;
use graft_core::{
    AdmissionSummary, ApplicationRef, Constraint, PatchId, Provenance, StateId, action_id,
    application_id, materialize_application,
};
use std::time::{SystemTime, UNIX_EPOCH};

#[cfg(unix)]
use std::ffi::OsString;
#[cfg(unix)]
use std::os::unix::ffi::OsStringExt;

#[test]
fn fetch_only_refuses_to_initialize_missing_remote() {
    let dir = test_dir("fetch-missing");
    let workspace = dir.join("workspace");
    let remote = dir.join("remote.git");
    fs::create_dir_all(&workspace).unwrap();

    let error = sync_public_store(&workspace, &remote, false, true)
        .unwrap_err()
        .to_string();

    assert!(error.contains("[E_SYNC_REMOTE_INVALID]"), "{error}");
    assert!(
        error.contains("fetch-only sync cannot initialize"),
        "{error}"
    );
    assert!(
        !remote.exists(),
        "fetch-only sync must not create a missing remote"
    );
    fs::remove_dir_all(dir).ok();
}

#[test]
fn fetch_only_does_not_create_remote_public_sidecar() {
    let dir = test_dir("fetch-existing-empty");
    let workspace = dir.join("workspace");
    let remote = dir.join("remote.git");
    fs::create_dir_all(&workspace).unwrap();
    gix::init_bare(&remote).unwrap();

    let report = sync_public_store(&workspace, &remote, false, true).unwrap();

    assert_eq!(report.fetched, 0);
    assert!(
        !remote.join("graft-public").exists(),
        "fetch-only sync must not create remote sidecar data"
    );
    fs::remove_dir_all(dir).ok();
}

#[test]
fn fetch_rejects_remote_public_files_without_manifest_head() {
    let dir = test_dir("fetch-public-without-manifest");
    let workspace = dir.join("workspace");
    let remote = dir.join("remote.git");
    fs::create_dir_all(&workspace).unwrap();
    gix::init_bare(&remote).unwrap();
    fs::create_dir_all(remote.join("graft-public").join("patch")).unwrap();
    fs::write(
        remote
            .join("graft-public")
            .join("patch")
            .join("patch:one.json"),
        "{}\n",
    )
    .unwrap();

    let error = sync_public_store(&workspace, &remote, false, true)
        .unwrap_err()
        .to_string();

    assert!(error.contains("[E_SYNC_MANIFEST_INVALID]"), "{error}");
    assert!(
        error.contains("remote public store has files but no manifest HEAD"),
        "{error}"
    );
    assert!(
        !workspace.join("store/public/patch/patch:one.json").exists(),
        "fetch must not copy uncheckpointed remote public files"
    );
    fs::remove_dir_all(dir).ok();
}

#[test]
fn public_store_validation_rejects_application_missing_action() {
    let dir = test_dir("application-missing-action");
    let public = dir.join("public");
    let application = write_application_objects(&public, "missing-action");
    let ApplicationRef::Stored(application_id) = application;
    let application_record = read_store_json::<ApplicationRecord>(
        &public
            .join("application")
            .join(format!("{application_id}.json")),
        "application",
    )
    .unwrap();
    fs::remove_file(
        public
            .join("action")
            .join(format!("{}.json", application_record.action)),
    )
    .unwrap();

    let error = validate_public_store_objects(&public)
        .unwrap_err()
        .to_string();

    assert!(error.contains("[E_SYNC_STORE_OBJECT_INVALID]"), "{error}");
    assert!(error.contains("references missing action"), "{error}");
    fs::remove_dir_all(dir).ok();
}

#[test]
fn public_store_validation_rejects_application_missing_change() {
    let dir = test_dir("application-missing-change");
    let public = dir.join("public");
    let application = write_application_objects(&public, "missing-change");
    let ApplicationRef::Stored(application_id) = application;
    let application_record = read_store_json::<ApplicationRecord>(
        &public
            .join("application")
            .join(format!("{application_id}.json")),
        "application",
    )
    .unwrap();
    fs::remove_file(
        public
            .join("change")
            .join(format!("{}.json", application_record.change)),
    )
    .unwrap();

    let error = validate_public_store_objects(&public)
        .unwrap_err()
        .to_string();

    assert!(error.contains("[E_SYNC_STORE_OBJECT_INVALID]"), "{error}");
    assert!(error.contains("references missing change"), "{error}");
    fs::remove_dir_all(dir).ok();
}

#[test]
fn public_store_validation_rejects_application_proof_mismatch() {
    let dir = test_dir("application-proof-mismatch");
    let public = dir.join("public");
    let application = write_application_objects(&public, "proof-mismatch");
    let ApplicationRef::Stored(old_application_id) = application;
    let old_path = public
        .join("application")
        .join(format!("{old_application_id}.json"));
    let mut application_record =
        read_store_json::<ApplicationRecord>(&old_path, "application").unwrap();
    fs::remove_file(old_path).unwrap();
    application_record.applicability_proof.action = graft_core::ActionId::new("action:wrong");
    let new_application_id = application_id(&application_record).unwrap();
    fs::write(
        public
            .join("application")
            .join(format!("{new_application_id}.json")),
        serde_json::to_vec(&application_record).unwrap(),
    )
    .unwrap();

    let error = validate_public_store_objects(&public)
        .unwrap_err()
        .to_string();

    assert!(error.contains("[E_SYNC_STORE_OBJECT_INVALID]"), "{error}");
    assert!(error.contains("proof action"), "{error}");
    fs::remove_dir_all(dir).ok();
}

#[test]
fn push_sync_initializes_missing_remote() {
    let dir = test_dir("push-missing");
    let workspace = dir.join("workspace");
    let remote = dir.join("remote.git");
    fs::create_dir_all(workspace.join("store").join("public")).unwrap();

    let report = sync_public_store(&workspace, &remote, true, false).unwrap();

    assert!(remote.join("HEAD").exists());
    assert!(remote.join("graft-public").exists());
    assert_eq!(report.pushed, 0);
    assert!(report.facts_tip.is_some());
    assert!(report.blobs_tip.is_some());
    assert!(report.manifest_id.is_some());
    fs::remove_dir_all(dir).ok();
}

#[test]
fn fetch_rejects_v1_manifest_version() {
    let dir = test_dir("fetch-v1-manifest-version");
    let source = dir.join("source");
    let dest = dir.join("dest");
    let remote = dir.join("remote.git");
    let source_public = source.join("store").join("public");
    let patch = write_valid_patch_object(&source_public, "v1-manifest-version");
    sync_public_store(&source, &remote, true, false).unwrap();

    let remote_public = remote.join("graft-public");
    let manifest_id = read_manifest_id(&remote_public).unwrap().unwrap();
    let manifest_path = remote_public
        .join("manifest")
        .join(format!("{manifest_id}.json"));
    let mut manifest: ManifestRecord =
        serde_json::from_slice(&fs::read(&manifest_path).unwrap()).unwrap();
    manifest.version = 1;
    rewrite_manifest_head(&remote_public, manifest);

    let error = sync_public_store(&dest, &remote, false, true)
        .unwrap_err()
        .to_string();

    assert!(error.contains("[E_SYNC_MANIFEST_INVALID]"), "{error}");
    assert!(
        error.contains("unsupported manifest version 1; expected 2"),
        "{error}"
    );
    assert!(
        !dest
            .join("store/public/patch")
            .join(format!("{patch}.json"))
            .exists(),
        "fetch must not copy objects from a v1 manifest"
    );
    fs::remove_dir_all(dir).ok();
}

#[test]
fn push_sync_refuses_non_empty_non_git_remote() {
    let dir = test_dir("non-empty-remote");
    let workspace = dir.join("workspace");
    let remote = dir.join("remote.git");
    fs::create_dir_all(workspace.join("store").join("public")).unwrap();
    fs::create_dir_all(&remote).unwrap();
    fs::write(remote.join("README.md"), "not a git repo\n").unwrap();

    let error = sync_public_store(&workspace, &remote, true, false)
        .unwrap_err()
        .to_string();

    assert!(error.contains("[E_SYNC_REMOTE_INVALID]"), "{error}");
    assert!(error.contains("not empty enough to initialize"), "{error}");
    assert!(!remote.join("HEAD").exists());
    assert_eq!(
        fs::read_to_string(remote.join("README.md")).unwrap(),
        "not a git repo\n"
    );
    fs::remove_dir_all(dir).ok();
}

#[test]
fn fetch_rejects_manifest_tip_that_is_not_in_remote_object_database() {
    let dir = test_dir("fetch-missing-tip-object");
    let source = dir.join("source");
    let dest = dir.join("dest");
    let remote = dir.join("remote.git");
    let source_public = source.join("store").join("public");
    let patch = write_valid_patch_object(&source_public, "missing-tip-object");
    sync_public_store(&source, &remote, true, false).unwrap();

    let remote_public = remote.join("graft-public");
    let manifest_id = read_manifest_id(&remote_public).unwrap().unwrap();
    let manifest_path = remote_public
        .join("manifest")
        .join(format!("{manifest_id}.json"));
    let mut manifest: ManifestRecord =
        serde_json::from_slice(&fs::read(&manifest_path).unwrap()).unwrap();
    manifest.facts_tip = "0000000000000000000000000000000000000000".to_string();
    rewrite_manifest_head(&remote_public, manifest);

    let error = sync_public_store(&dest, &remote, false, true)
        .unwrap_err()
        .to_string();

    assert!(error.contains("[E_SYNC_MANIFEST_INVALID]"), "{error}");
    assert!(
        error.contains("facts_tip `0000000000000000000000000000000000000000` does not exist"),
        "{error}"
    );
    assert!(
        !dest
            .join("store/public/patch")
            .join(format!("{patch}.json"))
            .exists(),
        "fetch must not copy objects from an invalid manifest"
    );
    fs::remove_dir_all(dir).ok();
}

#[test]
fn fetch_rejects_manifest_tip_that_does_not_match_partition_ref() {
    let dir = test_dir("fetch-tip-ref-mismatch");
    let source = dir.join("source");
    let dest = dir.join("dest");
    let remote = dir.join("remote.git");
    let source_public = source.join("store").join("public");
    let patch = write_valid_patch_object(&source_public, "tip-ref-mismatch");
    write_valid_blob_object(&source_public, b"blob\n");
    sync_public_store(&source, &remote, true, false).unwrap();

    let remote_public = remote.join("graft-public");
    let manifest_id = read_manifest_id(&remote_public).unwrap().unwrap();
    let manifest_path = remote_public
        .join("manifest")
        .join(format!("{manifest_id}.json"));
    let mut manifest: ManifestRecord =
        serde_json::from_slice(&fs::read(&manifest_path).unwrap()).unwrap();
    manifest.facts_tip = manifest.blobs_tip.clone();
    rewrite_manifest_head(&remote_public, manifest);

    let error = sync_public_store(&dest, &remote, false, true)
        .unwrap_err()
        .to_string();

    assert!(error.contains("[E_SYNC_MANIFEST_INVALID]"), "{error}");
    assert!(error.contains("refs/graft/facts points to"), "{error}");
    assert!(
        !dest
            .join("store/public/patch")
            .join(format!("{patch}.json"))
            .exists(),
        "fetch must not copy objects from a manifest/ref mismatch"
    );
    fs::remove_dir_all(dir).ok();
}

#[test]
fn read_manifest_id_accepts_missing_or_valid_manifest_head() {
    let dir = test_dir("manifest-head-valid");
    let remote_public = dir.join("graft-public");

    assert_eq!(read_manifest_id(&remote_public).unwrap(), None);

    fs::create_dir_all(remote_public.join("manifest")).unwrap();
    fs::write(
        remote_public.join("manifest").join("HEAD"),
        "manifest:abc123def456\n",
    )
    .unwrap();

    assert_eq!(
        read_manifest_id(&remote_public).unwrap().as_deref(),
        Some("manifest:abc123def456")
    );
    fs::remove_dir_all(dir).ok();
}

#[test]
fn read_manifest_id_rejects_empty_or_malformed_manifest_head() {
    for (label, value, expected) in [
        ("empty", "", "expected manifest:<digest>"),
        ("blank", " \n", "expected manifest:<digest>"),
        (
            "wrong-prefix",
            "patch:abc123def456",
            "expected manifest:<digest>",
        ),
        (
            "missing-digest",
            "manifest:",
            "digest must be 12 lowercase hex",
        ),
        (
            "short-digest",
            "manifest:abc123",
            "digest must be 12 lowercase hex",
        ),
        (
            "uppercase-digest",
            "manifest:ABC123DEF456",
            "digest must be 12 lowercase hex",
        ),
    ] {
        let dir = test_dir(label);
        let remote_public = dir.join("graft-public");
        fs::create_dir_all(remote_public.join("manifest")).unwrap();
        fs::write(remote_public.join("manifest").join("HEAD"), value).unwrap();

        let error = read_manifest_id(&remote_public).unwrap_err().to_string();

        assert!(error.contains("[E_SYNC_MANIFEST_HEAD_INVALID]"), "{error}");
        assert!(error.contains(expected), "{error}");
        fs::remove_dir_all(dir).ok();
    }
}

#[test]
fn digest_relative_path_rejects_paths_outside_root() {
    let root = PathBuf::from("/tmp/graft-sync-root");
    let outside = PathBuf::from("/tmp/graft-sync-other/blob");

    let error = digest_relative_path(&root, &outside)
        .unwrap_err()
        .to_string();

    assert!(error.contains("[E_SYNC_STORE_PATH_INVALID]"), "{error}");
    assert!(error.contains("not under digest root"), "{error}");
}

#[cfg(unix)]
#[test]
fn digest_relative_path_rejects_non_utf8_store_paths() {
    let root = PathBuf::from("/tmp/graft-sync-root");
    let path = root.join(OsString::from_vec(b"tree-\xFF.json".to_vec()));

    let error = digest_relative_path(&root, &path).unwrap_err().to_string();

    assert!(error.contains("[E_SYNC_STORE_PATH_INVALID]"), "{error}");
    assert!(error.contains("valid UTF-8"), "{error}");
}

#[test]
fn facts_partition_excludes_blob_and_manifest_sidecars() {
    let dir = test_dir("facts-partition");
    let public = dir.join("graft-public");
    fs::create_dir_all(public.join("patch")).unwrap();
    fs::create_dir_all(public.join("blob")).unwrap();
    fs::create_dir_all(public.join("manifest")).unwrap();
    fs::write(public.join("patch").join("patch:one.json"), "{}\n").unwrap();
    fs::write(public.join("blob").join("deadbeef"), "blob-v1\n").unwrap();
    fs::write(
        public.join("manifest").join("HEAD"),
        "manifest:abc123def456\n",
    )
    .unwrap();

    let initial_digest = digest_public_partition(&public, PublicPartition::Facts).unwrap();
    assert_eq!(count_public_facts_files(&public).unwrap(), 1);
    assert_eq!(count_files(&public.join("blob")).unwrap(), 1);

    fs::write(public.join("blob").join("deadbeef"), "blob-v2\n").unwrap();
    fs::write(
        public.join("manifest").join("HEAD"),
        "manifest:def456abc123\n",
    )
    .unwrap();
    assert_eq!(
        digest_public_partition(&public, PublicPartition::Facts).unwrap(),
        initial_digest,
        "facts digest must ignore blob and manifest sidecars"
    );

    fs::write(
        public.join("patch").join("patch:one.json"),
        "{\"changed\":true}\n",
    )
    .unwrap();
    assert_ne!(
        digest_public_partition(&public, PublicPartition::Facts).unwrap(),
        initial_digest,
        "facts digest must still track fact object changes"
    );

    fs::remove_dir_all(dir).ok();
}

#[test]
fn push_manifest_summary_keeps_facts_and_blobs_separate() {
    let dir = test_dir("manifest-summary-partitions");
    let workspace = dir.join("workspace");
    let remote = dir.join("remote.git");
    let local_public = workspace.join("store").join("public");
    let first = sync_public_store(&workspace, &remote, true, false).unwrap();
    write_valid_patch_object(&local_public, "manifest-summary");
    write_valid_blob_object(&local_public, b"blob\n");
    let remote_public = remote.join("graft-public");

    let report = sync_public_store(&workspace, &remote, true, false).unwrap();
    let manifest_id = report.manifest_id.unwrap();
    let manifest: ManifestRecord = serde_json::from_slice(
        &fs::read(
            remote_public
                .join("manifest")
                .join(format!("{manifest_id}.json")),
        )
        .unwrap(),
    )
    .unwrap();

    assert_eq!(
        manifest.prev_manifest.as_deref(),
        first.manifest_id.as_deref()
    );
    assert_eq!(manifest.summary.facts_files, 5);
    assert_eq!(manifest.summary.blob_files, 1);
    fs::remove_dir_all(dir).ok();
}

#[test]
fn fetch_records_remote_last_synced() {
    let dir = test_dir("fetch-records-last-synced");
    let source = dir.join("source");
    let dest = dir.join("dest");
    let remote = dir.join("remote.git");
    let source_public = source.join("store").join("public");
    let patch = write_valid_patch_object(&source_public, "fetch-last-synced");
    let pushed = sync_public_store(&source, &remote, true, false).unwrap();
    let pushed_manifest = pushed.manifest_id.unwrap();

    let fetched = sync_public_store(&dest, &remote, false, true).unwrap();

    assert_eq!(fetched.previous_last_synced, None);
    assert_eq!(
        fetched.last_synced.as_deref(),
        Some(pushed_manifest.as_str())
    );
    assert!(fetched.state_changed);
    assert_eq!(
        read_remote_last_synced(&dest, &remote).unwrap().as_deref(),
        Some(pushed_manifest.as_str())
    );
    assert!(
        dest.join("store/public/patch")
            .join(format!("{patch}.json"))
            .exists()
    );
    fs::remove_dir_all(dir).ok();
}

#[test]
fn push_only_updates_last_synced_when_remote_is_at_recorded_tip() {
    let dir = test_dir("push-recorded-tip");
    let workspace = dir.join("workspace");
    let remote = dir.join("remote.git");
    let local_public = workspace.join("store").join("public");
    write_valid_patch_object(&local_public, "push-first");
    let first = sync_public_store(&workspace, &remote, true, false).unwrap();
    let first_manifest = first.manifest_id.unwrap();
    assert_eq!(
        read_remote_last_synced(&workspace, &remote)
            .unwrap()
            .as_deref(),
        Some(first_manifest.as_str())
    );

    write_valid_patch_object(&local_public, "push-second");
    let second = sync_public_store(&workspace, &remote, true, false).unwrap();
    let second_manifest = second.manifest_id.unwrap();
    let remote_public = remote.join("graft-public");
    let manifest = read_manifest_record(&manifest_path(&remote_public, &second_manifest)).unwrap();

    assert_eq!(
        second.previous_last_synced.as_deref(),
        Some(first_manifest.as_str())
    );
    assert_eq!(
        second.last_synced.as_deref(),
        Some(second_manifest.as_str())
    );
    assert_eq!(
        manifest.prev_manifest.as_deref(),
        Some(first_manifest.as_str())
    );
    assert!(
        workspace
            .join("store/public/manifest")
            .join(format!("{second_manifest}.json"))
            .exists(),
        "push-only must keep the local manifest sidecar for the checkpoint it produced"
    );
    fs::remove_dir_all(dir).ok();
}

#[test]
fn push_only_rejects_remote_ahead_of_recorded_last_synced() {
    let dir = test_dir("push-remote-ahead");
    let source = dir.join("source");
    let dest = dir.join("dest");
    let remote = dir.join("remote.git");
    write_valid_patch_object(&source.join("store/public"), "first");
    let first = sync_public_store(&source, &remote, true, false).unwrap();
    sync_public_store(&dest, &remote, false, true).unwrap();
    write_valid_patch_object(&source.join("store/public"), "second");
    let second = sync_public_store(&source, &remote, true, false).unwrap();
    write_valid_patch_object(&dest.join("store/public"), "dest-local");

    let error = sync_public_store(&dest, &remote, true, false)
        .unwrap_err()
        .to_string();

    assert!(error.contains("[E_SYNC_DIVERGENCE]"), "{error}");
    assert!(error.contains("fetch before --push-only"), "{error}");
    assert!(
        error.contains(first.manifest_id.as_deref().unwrap()),
        "{error}"
    );
    assert!(
        error.contains(second.manifest_id.as_deref().unwrap()),
        "{error}"
    );
    fs::remove_dir_all(dir).ok();
}

#[test]
fn push_rejects_existing_remote_without_recorded_common_manifest() {
    let dir = test_dir("push-no-common-last-synced");
    let source = dir.join("source");
    let fresh = dir.join("fresh");
    let remote = dir.join("remote.git");
    write_valid_patch_object(&source.join("store/public"), "remote-history");
    let remote_report = sync_public_store(&source, &remote, true, false).unwrap();
    write_valid_patch_object(&fresh.join("store/public"), "fresh-local");

    let error = sync_public_store(&fresh, &remote, true, true)
        .unwrap_err()
        .to_string();

    assert!(error.contains("[E_SYNC_DIVERGENCE]"), "{error}");
    assert!(error.contains("no recorded last_synced"), "{error}");
    assert!(
        error.contains(remote_report.manifest_id.as_deref().unwrap()),
        "{error}"
    );
    fs::remove_dir_all(dir).ok();
}

#[test]
fn keep_remote_accepts_remote_tip_without_pushing_local_objects() {
    let dir = test_dir("keep-remote-fresh-local");
    let source = dir.join("source");
    let fresh = dir.join("fresh");
    let remote = dir.join("remote.git");
    let remote_patch = write_valid_patch_object(&source.join("store/public"), "remote");
    let local_patch = write_valid_patch_object(&fresh.join("store/public"), "fresh-local");
    let remote_report = sync_public_store(&source, &remote, true, false).unwrap();
    let remote_manifest = remote_report.manifest_id.unwrap();

    let report = sync_public_store_with_options(
        &fresh,
        &remote,
        SyncOptions {
            push: true,
            fetch: true,
            on_divergence: DivergencePolicy::KeepRemote,
        },
    )
    .unwrap();

    assert_eq!(report.previous_last_synced, None);
    assert_eq!(report.pushed, 0, "keep-remote must not write local objects");
    assert!(
        report.fetched > 0,
        "keep-remote must fetch the remote object frontier"
    );
    assert_eq!(report.manifest_id, None);
    assert_eq!(
        report.last_synced.as_deref(),
        Some(remote_manifest.as_str())
    );
    assert_eq!(
        read_remote_last_synced(&fresh, &remote).unwrap().as_deref(),
        Some(remote_manifest.as_str())
    );
    assert_eq!(
        read_manifest_id(&remote.join("graft-public"))
            .unwrap()
            .as_deref(),
        Some(remote_manifest.as_str()),
        "keep-remote must not advance remote manifest HEAD"
    );
    assert!(
        fresh
            .join("store/public/patch")
            .join(format!("{remote_patch}.json"))
            .exists(),
        "keep-remote must still accept remote objects locally"
    );
    assert!(
        !remote
            .join("graft-public/patch")
            .join(format!("{local_patch}.json"))
            .exists(),
        "keep-remote must not publish local divergent objects"
    );
    fs::remove_dir_all(dir).ok();
}

#[test]
fn keep_remote_rejects_push_only_divergence() {
    let dir = test_dir("keep-remote-push-only");
    let source = dir.join("source");
    let dest = dir.join("dest");
    let remote = dir.join("remote.git");
    write_valid_patch_object(&source.join("store/public"), "first");
    sync_public_store(&source, &remote, true, false).unwrap();
    sync_public_store(&dest, &remote, false, true).unwrap();
    write_valid_patch_object(&source.join("store/public"), "second");
    sync_public_store(&source, &remote, true, false).unwrap();
    write_valid_patch_object(&dest.join("store/public"), "dest-local");

    let error = sync_public_store_with_options(
        &dest,
        &remote,
        SyncOptions {
            push: true,
            fetch: false,
            on_divergence: DivergencePolicy::KeepRemote,
        },
    )
    .unwrap_err()
    .to_string();

    assert!(error.contains("[E_SYNC_DIVERGENCE]"), "{error}");
    assert!(error.contains("keep-remote requires fetch"), "{error}");
    fs::remove_dir_all(dir).ok();
}

#[test]
fn push_rejects_existing_manifest_head_without_valid_chain() {
    let dir = test_dir("push-invalid-prev-manifest");
    let workspace = dir.join("workspace");
    let remote = dir.join("remote.git");
    let remote_public = remote.join("graft-public");
    fs::create_dir_all(workspace.join("store").join("public")).unwrap();
    gix::init_bare(&remote).unwrap();
    fs::create_dir_all(remote_public.join("manifest")).unwrap();
    fs::write(
        remote_public.join("manifest").join("HEAD"),
        "manifest:abc123def456\n",
    )
    .unwrap();
    fs::write(
        remote_public
            .join("manifest")
            .join("manifest:abc123def456.json"),
        "{}\n",
    )
    .unwrap();

    let error = sync_public_store(&workspace, &remote, true, false)
        .unwrap_err()
        .to_string();

    assert!(error.contains("[E_SYNC_MANIFEST_INVALID]"), "{error}");
    assert!(error.contains("missing field"), "{error}");
    fs::remove_dir_all(dir).ok();
}

#[test]
fn fetch_rejects_manifest_body_that_does_not_match_canonical_id() {
    let dir = test_dir("fetch-tampered-manifest");
    let source = dir.join("source");
    let dest = dir.join("dest");
    let remote = dir.join("remote.git");
    let source_public = source.join("store").join("public");
    let patch = write_valid_patch_object(&source_public, "tampered-manifest");
    sync_public_store(&source, &remote, true, false).unwrap();

    let remote_public = remote.join("graft-public");
    let manifest_id = read_manifest_id(&remote_public).unwrap().unwrap();
    let manifest_path = manifest_path(&remote_public, &manifest_id);
    let mut manifest = read_manifest_record(&manifest_path).unwrap();
    manifest.summary.facts_files += 1;
    fs::write(
        &manifest_path,
        serde_json::to_vec_pretty(&manifest).unwrap(),
    )
    .unwrap();

    let error = sync_public_store(&dest, &remote, false, true)
        .unwrap_err()
        .to_string();

    assert!(error.contains("[E_SYNC_MANIFEST_INVALID]"), "{error}");
    assert!(
        error.contains("does not match canonical body id"),
        "{error}"
    );
    assert!(
        !dest
            .join("store/public/patch")
            .join(format!("{patch}.json"))
            .exists(),
        "fetch must reject a tampered manifest before copying public objects"
    );
    fs::remove_dir_all(dir).ok();
}

#[test]
fn fetch_rejects_manifest_prev_that_is_missing() {
    let dir = test_dir("fetch-missing-prev-manifest");
    let source = dir.join("source");
    let dest = dir.join("dest");
    let remote = dir.join("remote.git");
    let source_public = source.join("store").join("public");
    let patch = write_valid_patch_object(&source_public, "missing-prev-manifest");
    sync_public_store(&source, &remote, true, false).unwrap();

    let remote_public = remote.join("graft-public");
    let manifest_id = read_manifest_id(&remote_public).unwrap().unwrap();
    let manifest_path = manifest_path(&remote_public, &manifest_id);
    let mut manifest = read_manifest_record(&manifest_path).unwrap();
    manifest.prev_manifest = Some("manifest:abc123def456".to_string());
    rewrite_manifest_head(&remote_public, manifest);

    let error = sync_public_store(&dest, &remote, false, true)
        .unwrap_err()
        .to_string();

    assert!(error.contains("[E_SYNC_MANIFEST_INVALID]"), "{error}");
    assert!(
        error.contains("prev_manifest `manifest:abc123def456` is missing"),
        "{error}"
    );
    assert!(
        !dest
            .join("store/public/patch")
            .join(format!("{patch}.json"))
            .exists(),
        "fetch must reject a broken manifest chain before copying public objects"
    );
    fs::remove_dir_all(dir).ok();
}

#[test]
fn fetch_rejects_typed_object_body_that_does_not_match_filename() {
    let dir = test_dir("fetch-invalid-typed-object");
    let source = dir.join("source");
    let dest = dir.join("dest");
    let remote = dir.join("remote.git");
    let source_public = source.join("store").join("public");
    let mut patch = valid_patch_record("remote-tamper", &source_public);
    write_patch_object(&source_public, &patch);
    sync_public_store(&source, &remote, true, false).unwrap();

    patch.provenance.message = Some("tampered".to_string());
    let remote_patch = remote
        .join("graft-public")
        .join("patch")
        .join(format!("{}.json", patch.id));
    fs::write(&remote_patch, serde_json::to_vec_pretty(&patch).unwrap()).unwrap();

    let error = sync_public_store(&dest, &remote, false, true)
        .unwrap_err()
        .to_string();

    assert!(error.contains("[E_SYNC_STORE_OBJECT_INVALID]"), "{error}");
    assert!(error.contains("patch body id"), "{error}");
    assert!(
        !dest
            .join("store/public/patch")
            .join(format!("{}.json", patch.id))
            .exists(),
        "fetch must reject a bad typed object before copying it"
    );
    fs::remove_dir_all(dir).ok();
}

#[test]
fn fetch_rejects_same_immutable_id_with_different_local_bytes() {
    let dir = test_dir("fetch-same-id-different-bytes");
    let source = dir.join("source");
    let dest = dir.join("dest");
    let remote = dir.join("remote.git");
    let source_public = source.join("store").join("public");
    let patch = valid_patch_record("local-conflict", &source_public);
    write_patch_object(&source_public, &patch);
    sync_public_store(&source, &remote, true, false).unwrap();

    let local_public = dest.join("store").join("public");
    let mut local_patch = patch.clone();
    local_patch.provenance.created_at = "different-local-display-time".to_string();
    write_patch_object(&local_public, &local_patch);
    let local_patch_path = local_public
        .join("patch")
        .join(format!("{}.json", local_patch.id));
    let before = fs::read(&local_patch_path).unwrap();

    let error = sync_public_store(&dest, &remote, false, true)
        .unwrap_err()
        .to_string();

    assert!(error.contains("[E_SYNC_STORE_OBJECT_INVALID]"), "{error}");
    assert!(
        error.contains("destination already has different bytes for immutable public object"),
        "{error}"
    );
    assert_eq!(
        fs::read(&local_patch_path).unwrap(),
        before,
        "fetch must not overwrite an existing immutable object with the same id"
    );
    fs::remove_dir_all(dir).ok();
}

#[test]
fn merge_evidence_refs_unions_typed_records_and_keeps_newest_updated_at() {
    let dir = test_dir("union");
    fs::create_dir_all(&dir).unwrap();
    let source = dir.join("source").join("patch:one.json");
    let dest = dir.join("dest").join("patch:one.json");
    fs::create_dir_all(source.parent().unwrap()).unwrap();
    fs::create_dir_all(dest.parent().unwrap()).unwrap();
    fs::write(
        &source,
        r#"{"owner":"patch:one","evidence":["ev:src"],"updated_at":"2026-06-03T10:00:00Z"}"#,
    )
    .unwrap();
    fs::write(
        &dest,
        r#"{"owner":"patch:one","evidence":["ev:dst"],"updated_at":"2026-06-03T09:00:00Z"}"#,
    )
    .unwrap();

    assert!(merge_evidence_refs_file(&source, &dest).unwrap());

    let refs = read_evidence_refs_file(&dest).unwrap();
    assert_eq!(refs.owner, "patch:one");
    assert_eq!(refs.evidence, vec!["ev:dst", "ev:src"]);
    assert_eq!(refs.updated_at.as_deref(), Some("2026-06-03T10:00:00Z"));
    fs::remove_dir_all(dir).ok();
}

#[test]
fn merge_evidence_refs_rejects_legacy_array() {
    let dir = test_dir("legacy-array");
    fs::create_dir_all(&dir).unwrap();
    let source = dir.join("source").join("patch:one.json");
    let dest = dir.join("dest").join("patch:one.json");
    fs::create_dir_all(source.parent().unwrap()).unwrap();
    fs::create_dir_all(dest.parent().unwrap()).unwrap();
    fs::write(&source, r#"["ev:src"]"#).unwrap();
    fs::write(
        &dest,
        r#"{"owner":"patch:one","evidence":["ev:dst"],"updated_at":null}"#,
    )
    .unwrap();

    let error = merge_evidence_refs_file(&source, &dest)
        .unwrap_err()
        .to_string();

    assert!(error.contains("invalid evidence refs"), "{error}");
    assert!(
        error.contains("expected evidence refs object with owner and evidence fields"),
        "{error}"
    );
    fs::remove_dir_all(dir).ok();
}

#[test]
fn merge_evidence_refs_rejects_missing_evidence_array() {
    let dir = test_dir("missing-evidence");
    fs::create_dir_all(&dir).unwrap();
    let source = dir.join("source").join("patch:one.json");
    let dest = dir.join("dest").join("patch:one.json");
    fs::create_dir_all(source.parent().unwrap()).unwrap();
    fs::create_dir_all(dest.parent().unwrap()).unwrap();
    fs::write(
        &source,
        r#"{"owner":"patch:one","updated_at":"2026-06-03T10:00:00Z"}"#,
    )
    .unwrap();
    fs::write(
        &dest,
        r#"{"owner":"patch:one","evidence":["ev:dst"],"updated_at":null}"#,
    )
    .unwrap();

    let error = merge_evidence_refs_file(&source, &dest)
        .unwrap_err()
        .to_string();
    assert!(error.contains("missing field `evidence`"), "{error}");
    fs::remove_dir_all(dir).ok();
}

#[test]
fn merge_evidence_refs_rejects_owner_mismatch() {
    let dir = test_dir("owner-mismatch");
    fs::create_dir_all(&dir).unwrap();
    let source = dir.join("source").join("patch:one.json");
    let dest = dir.join("dest").join("patch:one.json");
    fs::create_dir_all(source.parent().unwrap()).unwrap();
    fs::create_dir_all(dest.parent().unwrap()).unwrap();
    fs::write(
        &source,
        r#"{"owner":"patch:other","evidence":["ev:src"],"updated_at":null}"#,
    )
    .unwrap();
    fs::write(
        &dest,
        r#"{"owner":"patch:one","evidence":["ev:dst"],"updated_at":null}"#,
    )
    .unwrap();

    let error = merge_evidence_refs_file(&source, &dest)
        .unwrap_err()
        .to_string();
    assert!(error.contains("owner `patch:other`"), "{error}");
    fs::remove_dir_all(dir).ok();
}

fn test_dir(name: &str) -> std::path::PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    std::env::temp_dir().join(format!("graft-sync-{name}-{}-{nanos}", std::process::id()))
}

fn write_valid_patch_object(public: &Path, message: &str) -> String {
    let patch = valid_patch_record(message, public);
    write_patch_object(public, &patch);
    patch.id.to_string()
}

fn write_application_objects(public: &Path, message: &str) -> ApplicationRef {
    let target = TreeSnapshot::new(vec![graft_core::TreeEntry {
        path: format!("{message}.txt"),
        hash: blake3_hex_digest(message.as_bytes()),
        size: message.len() as u64,
    }]);
    let materialized = materialize_application(
        StateId::GraftTree("tree:base".to_string()),
        None,
        StateId::GraftTree(target.id().unwrap()),
        &target,
    )
    .unwrap();
    let action_id = action_id(&materialized.action).unwrap();
    let application_id = materialized.record.id().unwrap();
    let change_id = materialized.change.id().unwrap();
    fs::create_dir_all(public.join("action")).unwrap();
    fs::create_dir_all(public.join("application")).unwrap();
    fs::create_dir_all(public.join("change")).unwrap();
    fs::create_dir_all(public.join("tree")).unwrap();
    fs::write(
        public.join("action").join(format!("{action_id}.json")),
        serde_json::to_vec(&materialized.action).unwrap(),
    )
    .unwrap();
    fs::write(
        public
            .join("application")
            .join(format!("{application_id}.json")),
        serde_json::to_vec(&materialized.record).unwrap(),
    )
    .unwrap();
    fs::write(
        public.join("change").join(format!("{change_id}.json")),
        serde_json::to_vec(&materialized.change).unwrap(),
    )
    .unwrap();
    fs::write(
        public
            .join("tree")
            .join(format!("{}.json", target.id().unwrap())),
        serde_json::to_vec(&target).unwrap(),
    )
    .unwrap();
    ApplicationRef::Stored(application_id)
}

fn valid_patch_record(message: &str, public: &Path) -> PatchRecord {
    let application = write_application_objects(public, message);
    let mut patch = PatchRecord {
        id: PatchId::new("patch:pending"),
        application,
        constraint: Constraint::Top,
        provenance: Provenance {
            producer: "graft-sync-test".to_string(),
            message: Some(message.to_string()),
            created_at: "2026-06-04T00:00:00Z".to_string(),
        },
        admission: AdmissionSummary {
            constraint: Constraint::Top,
        },
    };
    patch.id = patch_id(&patch).unwrap();
    patch
}

fn write_patch_object(public: &Path, patch: &PatchRecord) {
    let dir = public.join("patch");
    fs::create_dir_all(&dir).unwrap();
    fs::write(
        dir.join(format!("{}.json", patch.id)),
        serde_json::to_vec_pretty(patch).unwrap(),
    )
    .unwrap();
}

fn write_valid_blob_object(public: &Path, bytes: &[u8]) -> String {
    let hash = blake3_hex_digest(bytes);
    let dir = public.join("blob");
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join(&hash), bytes).unwrap();
    hash
}

fn rewrite_manifest_head(remote_public: &Path, mut manifest: ManifestRecord) -> ManifestRecord {
    manifest.id = expected_manifest_id(&manifest).unwrap();
    let dir = remote_public.join("manifest");
    fs::create_dir_all(&dir).unwrap();
    fs::write(
        dir.join(format!("{}.json", manifest.id)),
        serde_json::to_vec_pretty(&manifest).unwrap(),
    )
    .unwrap();
    fs::write(dir.join("HEAD"), &manifest.id).unwrap();
    manifest
}
