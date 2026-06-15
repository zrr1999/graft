use std::fs;
use std::path::{Path, PathBuf};

mod manifest;
mod progress;
mod public_store;
#[cfg(test)]
use graft_core::{ApplicationRecord, PatchRecord, TreeSnapshot, blake3_hex_digest, patch_id};
#[cfg(test)]
use manifest::*;
use manifest::{
    PublicPartition, validate_latest_manifest, write_manifest, write_manifest_sidecar,
    write_partition_ref,
};
use progress::{
    SyncProgressInput, read_remote_last_synced, validate_sync_progress, write_remote_last_synced,
};
#[cfg(test)]
use public_store::*;
use public_store::{copy_public_tree, validate_public_store_objects};

#[derive(Clone, Debug, Default)]
pub struct GraftSyncTransport;

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct SyncReport {
    pub pushed: usize,
    pub fetched: usize,
    pub facts_tip: Option<String>,
    pub blobs_tip: Option<String>,
    pub manifest_id: Option<String>,
    pub previous_last_synced: Option<String>,
    pub last_synced: Option<String>,
    pub state_changed: bool,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum DivergencePolicy {
    #[default]
    Abort,
    KeepRemote,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SyncOptions {
    pub push: bool,
    pub fetch: bool,
    pub on_divergence: DivergencePolicy,
}

impl SyncOptions {
    pub fn new(push: bool, fetch: bool) -> Self {
        Self {
            push,
            fetch,
            on_divergence: DivergencePolicy::Abort,
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum SyncError {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("gix error: {0}")]
    Gix(String),
    #[error("core error: {0}")]
    Core(#[from] graft_core::CoreError),
    #[error("[E_SYNC_REMOTE_INVALID] invalid sync remote at {path}: {message}")]
    InvalidRemote { path: PathBuf, message: String },
    #[error("[E_SYNC_MANIFEST_HEAD_INVALID] invalid manifest HEAD at {path}: {message}")]
    InvalidManifestHead { path: PathBuf, message: String },
    #[error("[E_SYNC_MANIFEST_INVALID] invalid sync manifest at {path}: {message}")]
    InvalidManifest { path: PathBuf, message: String },
    #[error("invalid evidence refs at {path}: {message}")]
    InvalidEvidenceRefs { path: PathBuf, message: String },
    #[error("[E_SYNC_STORE_PATH_INVALID] invalid sync store path at {path}: {message}")]
    InvalidStorePath { path: PathBuf, message: String },
    #[error("[E_SYNC_STORE_OBJECT_INVALID] invalid sync store object at {path}: {message}")]
    InvalidStoreObject { path: PathBuf, message: String },
    #[error("[E_SYNC_STATE_INVALID] invalid sync state at {path}: {message}")]
    InvalidSyncState { path: PathBuf, message: String },
    #[error("[E_SYNC_DIVERGENCE] sync divergence at {path}: {message}")]
    Divergence { path: PathBuf, message: String },
}

pub type Result<T> = std::result::Result<T, SyncError>;

impl GraftSyncTransport {
    pub fn sync_public_store(
        &self,
        workspace_root: impl AsRef<Path>,
        remote: impl AsRef<Path>,
        push: bool,
        fetch: bool,
    ) -> Result<SyncReport> {
        sync_public_store(workspace_root.as_ref(), remote.as_ref(), push, fetch)
    }

    pub fn sync_public_store_with_options(
        &self,
        workspace_root: impl AsRef<Path>,
        remote: impl AsRef<Path>,
        options: SyncOptions,
    ) -> Result<SyncReport> {
        sync_public_store_with_options(workspace_root.as_ref(), remote.as_ref(), options)
    }
}

pub fn sync_public_store(
    workspace_root: &Path,
    remote: &Path,
    push: bool,
    fetch: bool,
) -> Result<SyncReport> {
    sync_public_store_with_options(workspace_root, remote, SyncOptions::new(push, fetch))
}

pub fn sync_public_store_with_options(
    workspace_root: &Path,
    remote: &Path,
    options: SyncOptions,
) -> Result<SyncReport> {
    let remote_repo = ensure_storage_repo(remote, options.push)?;
    let local_public = workspace_root.join("store").join("public");
    let remote_public = remote.join("graft-public");
    fs::create_dir_all(&local_public)?;

    let mut report = SyncReport::default();
    let previous_last_synced = read_remote_last_synced(workspace_root, remote)?;
    let remote_latest =
        validate_latest_manifest(&remote_repo, &remote_public)?.map(|manifest| manifest.id);
    let sync_plan = validate_sync_progress(SyncProgressInput {
        workspace_root,
        remote,
        local_last_synced: previous_last_synced.as_deref(),
        remote_latest: remote_latest.as_deref(),
        repo: &remote_repo,
        remote_public: &remote_public,
        push: options.push,
        fetch: options.fetch,
        on_divergence: options.on_divergence,
    })?;
    report.previous_last_synced = previous_last_synced.clone();
    let mut final_remote_latest = remote_latest;
    if sync_plan.push {
        validate_public_store_objects(&local_public)?;
        fs::create_dir_all(&remote_public)?;
        report.pushed += copy_public_tree(&local_public, &remote_public, true)?;
        let facts_tip = write_partition_ref(&remote_repo, &remote_public, PublicPartition::Facts)?;
        let blobs_tip = write_partition_ref(&remote_repo, &remote_public, PublicPartition::Blobs)?;
        let manifest = write_manifest(
            &remote_repo,
            &remote_public,
            facts_tip.clone(),
            blobs_tip.clone(),
        )?;
        write_manifest_sidecar(&local_public, &manifest)?;
        report.facts_tip = Some(facts_tip);
        report.blobs_tip = Some(blobs_tip);
        final_remote_latest = Some(manifest.id.clone());
        report.manifest_id = Some(manifest.id);
    }
    if sync_plan.fetch {
        validate_latest_manifest(&remote_repo, &remote_public)?;
        validate_public_store_objects(&remote_public)?;
        report.fetched += copy_public_tree(&remote_public, &local_public, true)?;
    }
    if let Some(manifest_id) = final_remote_latest {
        report.state_changed =
            write_remote_last_synced(workspace_root, remote, &manifest_id)? || report.state_changed;
        report.last_synced = Some(manifest_id);
    }
    Ok(report)
}

fn ensure_storage_repo(remote: &Path, allow_init: bool) -> Result<gix::Repository> {
    match gix::open(remote) {
        Ok(repo) => Ok(repo),
        Err(open_error) => {
            if allow_init && remote_can_be_initialized(remote)? {
                fs::create_dir_all(remote)?;
                return gix::init_bare(remote).map_err(|err| SyncError::Gix(err.to_string()));
            }
            Err(SyncError::InvalidRemote {
                path: remote.to_path_buf(),
                message: if allow_init {
                    format!(
                        "not an existing Git repository and not empty enough to initialize: {open_error}"
                    )
                } else {
                    format!(
                        "not an existing Git repository; fetch-only sync cannot initialize remotes: {open_error}"
                    )
                },
            })
        }
    }
}

fn remote_can_be_initialized(remote: &Path) -> Result<bool> {
    let metadata = match fs::metadata(remote) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(true),
        Err(error) => return Err(error.into()),
    };
    if !metadata.is_dir() {
        return Err(SyncError::InvalidRemote {
            path: remote.to_path_buf(),
            message: "path exists and is not a directory".to_string(),
        });
    }
    let mut entries = fs::read_dir(remote)?;
    Ok(entries.next().transpose()?.is_none())
}

#[cfg(test)]
mod tests;
