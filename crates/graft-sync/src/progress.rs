use std::fs;
use std::path::{Path, PathBuf};

use graft_core::blake3_hex_digest;

use crate::manifest::{collect_manifest_chain_ids, validate_manifest_id};
use crate::{DivergencePolicy, Result, SyncError};

pub(crate) struct SyncProgressInput<'a> {
    pub(crate) workspace_root: &'a Path,
    pub(crate) remote: &'a Path,
    pub(crate) local_last_synced: Option<&'a str>,
    pub(crate) remote_latest: Option<&'a str>,
    pub(crate) repo: &'a gix::Repository,
    pub(crate) remote_public: &'a Path,
    pub(crate) push: bool,
    pub(crate) fetch: bool,
    pub(crate) on_divergence: DivergencePolicy,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct SyncPlan {
    pub(crate) push: bool,
    pub(crate) fetch: bool,
}

pub(crate) fn validate_sync_progress(input: SyncProgressInput<'_>) -> Result<SyncPlan> {
    let state_path = remote_last_synced_path(input.workspace_root, input.remote);
    if let Some(last_synced) = input.local_last_synced {
        let Some(remote_latest) = input.remote_latest else {
            return handle_divergence(
                &state_path,
                input,
                format!("local last_synced is `{last_synced}`, but remote has no manifest HEAD"),
            );
        };
        let remote_history =
            collect_manifest_chain_ids(input.repo, input.remote_public, remote_latest)?;
        if !remote_history.contains(last_synced) {
            return handle_divergence(
                &state_path,
                input,
                format!(
                    "local last_synced `{last_synced}` is not in remote manifest history rooted at `{remote_latest}`"
                ),
            );
        }
    } else if input.push
        && let Some(remote_latest) = input.remote_latest
    {
        return handle_divergence(
            &state_path,
            input,
            format!(
                "remote already has manifest `{remote_latest}` but this workspace has no recorded last_synced; run graft sync --fetch-only first"
            ),
        );
    }

    if input.push
        && !input.fetch
        && let (Some(last_synced), Some(remote_latest)) =
            (input.local_last_synced, input.remote_latest)
        && last_synced != remote_latest
    {
        return handle_divergence(
            &state_path,
            input,
            format!(
                "remote is ahead of local last_synced `{last_synced}` at `{remote_latest}`; fetch before --push-only"
            ),
        );
    }
    Ok(SyncPlan {
        push: input.push,
        fetch: input.fetch,
    })
}

fn handle_divergence(
    state_path: &Path,
    input: SyncProgressInput<'_>,
    message: String,
) -> Result<SyncPlan> {
    match input.on_divergence {
        DivergencePolicy::Abort => Err(SyncError::Divergence {
            path: state_path.to_path_buf(),
            message,
        }),
        DivergencePolicy::KeepRemote if input.fetch && input.remote_latest.is_some() => {
            Ok(SyncPlan {
                push: false,
                fetch: true,
            })
        }
        DivergencePolicy::KeepRemote if !input.fetch => Err(SyncError::Divergence {
            path: state_path.to_path_buf(),
            message: format!(
                "{message}; --on-divergence keep-remote requires fetch, so it cannot be used with --push-only"
            ),
        }),
        DivergencePolicy::KeepRemote => Err(SyncError::Divergence {
            path: state_path.to_path_buf(),
            message: format!(
                "{message}; --on-divergence keep-remote requires a remote manifest HEAD to keep"
            ),
        }),
    }
}

pub(crate) fn read_remote_last_synced(
    workspace_root: &Path,
    remote: &Path,
) -> Result<Option<String>> {
    let path = remote_last_synced_path(workspace_root, remote);
    match fs::read_to_string(&path) {
        Ok(value) => {
            let value = value.trim();
            if value.is_empty() {
                return Err(SyncError::InvalidSyncState {
                    path,
                    message: "last_synced is empty".to_string(),
                });
            }
            validate_manifest_id(&path, "last_synced", value).map_err(|error| {
                SyncError::InvalidSyncState {
                    path: path.clone(),
                    message: error.to_string(),
                }
            })?;
            Ok(Some(value.to_string()))
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}

pub(crate) fn write_remote_last_synced(
    workspace_root: &Path,
    remote: &Path,
    manifest_id: &str,
) -> Result<bool> {
    let path = remote_last_synced_path(workspace_root, remote);
    validate_manifest_id(&path, "last_synced", manifest_id).map_err(|error| {
        SyncError::InvalidSyncState {
            path: path.clone(),
            message: error.to_string(),
        }
    })?;
    if matches!(fs::read_to_string(&path), Ok(current) if current.trim() == manifest_id) {
        return Ok(false);
    }
    let parent = path.parent().ok_or_else(|| SyncError::InvalidSyncState {
        path: path.clone(),
        message: "last_synced path has no parent".to_string(),
    })?;
    fs::create_dir_all(parent)?;
    fs::write(&path, format!("{manifest_id}\n"))?;
    Ok(true)
}

fn remote_last_synced_path(workspace_root: &Path, remote: &Path) -> PathBuf {
    workspace_root
        .join(".graft")
        .join("local")
        .join("remotes")
        .join(remote_state_key(remote))
        .join("last_synced")
}

fn remote_state_key(remote: &Path) -> String {
    let normalized = remote
        .canonicalize()
        .unwrap_or_else(|_| remote.to_path_buf())
        .to_string_lossy()
        .into_owned();
    blake3_hex_digest(normalized.as_bytes())[..12].to_string()
}
