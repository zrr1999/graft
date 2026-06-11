use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Component, Path, PathBuf};

use graft_core::{
    Action, ApplicationRecord, Change, PatchRecord, PatchRelation, PromotionRecord, PropertySpec,
    TreeSnapshot, action_id, application_id, blake3_hex_digest, patch_id, promotion_id,
    relation_id, stable_typed_id, validate_application_integrity,
};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

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

const MANIFEST_VERSION: u32 = 2;

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ManifestRecord {
    pub id: String,
    pub version: u32,
    pub facts_tip: String,
    pub blobs_tip: String,
    pub prev_manifest: Option<String>,
    pub summary: ManifestSummary,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ManifestSummary {
    pub facts_files: usize,
    pub blob_files: usize,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct EvidenceRefs {
    owner: String,
    evidence: Vec<String>,
    updated_at: Option<String>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PublicPartition {
    Facts,
    Blobs,
}

impl PublicPartition {
    fn label(self) -> &'static str {
        match self {
            Self::Facts => "facts",
            Self::Blobs => "blobs",
        }
    }

    fn ref_name(self) -> &'static str {
        match self {
            Self::Facts => "refs/graft/facts",
            Self::Blobs => "refs/graft/blobs",
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

struct SyncProgressInput<'a> {
    workspace_root: &'a Path,
    remote: &'a Path,
    local_last_synced: Option<&'a str>,
    remote_latest: Option<&'a str>,
    repo: &'a gix::Repository,
    remote_public: &'a Path,
    push: bool,
    fetch: bool,
    on_divergence: DivergencePolicy,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct SyncPlan {
    push: bool,
    fetch: bool,
}

fn validate_sync_progress(input: SyncProgressInput<'_>) -> Result<SyncPlan> {
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

fn read_remote_last_synced(workspace_root: &Path, remote: &Path) -> Result<Option<String>> {
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

fn write_remote_last_synced(
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

fn write_partition_ref(
    repo: &gix::Repository,
    remote_public: &Path,
    partition: PublicPartition,
) -> Result<String> {
    let digest = digest_public_partition(remote_public, partition)?;
    let body = serde_json::json!({
        "version": 1,
        "partition": partition.label(),
        "digest": digest,
        "written_at": time::OffsetDateTime::now_utc().to_string(),
    });
    let bytes = serde_json::to_vec_pretty(&body)?;
    let blob = repo
        .write_blob(bytes)
        .map_err(|err| SyncError::Gix(err.to_string()))?;
    update_ref(repo, partition.ref_name(), blob.to_string())?;
    Ok(blob.to_string())
}

fn write_manifest(
    repo: &gix::Repository,
    remote_public: &Path,
    facts_tip: String,
    blobs_tip: String,
) -> Result<ManifestRecord> {
    let summary = ManifestSummary {
        facts_files: count_public_facts_files(remote_public)?,
        blob_files: count_files(&remote_public.join("blob"))?,
    };
    let mut manifest = ManifestRecord {
        id: "manifest:pending".to_string(),
        version: MANIFEST_VERSION,
        facts_tip,
        blobs_tip,
        prev_manifest: read_valid_previous_manifest_id(repo, remote_public)?,
        summary,
    };
    manifest.id = expected_manifest_id(&manifest)?;
    write_manifest_sidecar(remote_public, &manifest)?;
    let blob = repo
        .write_blob(serde_json::to_vec_pretty(&manifest)?)
        .map_err(|err| SyncError::Gix(err.to_string()))?;
    update_ref(repo, "refs/graft/manifests", blob.to_string())?;
    Ok(manifest)
}

fn write_manifest_sidecar(public: &Path, manifest: &ManifestRecord) -> Result<()> {
    let expected = expected_manifest_id(manifest)?;
    if manifest.id != expected {
        return Err(SyncError::InvalidManifest {
            path: public
                .join("manifest")
                .join(format!("{}.json", manifest.id)),
            message: format!(
                "manifest body id `{}` does not match canonical body id `{expected}`",
                manifest.id
            ),
        });
    }
    let manifest_dir = public.join("manifest");
    fs::create_dir_all(&manifest_dir)?;
    fs::write(
        manifest_dir.join(format!("{}.json", manifest.id)),
        serde_json::to_vec_pretty(&manifest)?,
    )?;
    fs::write(manifest_dir.join("HEAD"), &manifest.id)?;
    Ok(())
}

fn read_manifest_id(remote_public: &Path) -> Result<Option<String>> {
    let path = remote_public.join("manifest").join("HEAD");
    match fs::read_to_string(&path) {
        Ok(value) => parse_manifest_head(&path, &value).map(Some),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}

fn read_valid_previous_manifest_id(
    repo: &gix::Repository,
    remote_public: &Path,
) -> Result<Option<String>> {
    let Some(manifest_id) = read_manifest_id(remote_public)? else {
        return Ok(None);
    };
    validate_manifest_chain(repo, remote_public, &manifest_id)?;
    Ok(Some(manifest_id))
}

fn manifest_path(remote_public: &Path, manifest_id: &str) -> PathBuf {
    remote_public
        .join("manifest")
        .join(format!("{manifest_id}.json"))
}

fn read_manifest_record(path: &Path) -> Result<ManifestRecord> {
    serde_json::from_slice(&fs::read(path)?).map_err(|error| SyncError::InvalidManifest {
        path: path.to_path_buf(),
        message: error.to_string(),
    })
}

fn expected_manifest_id(manifest: &ManifestRecord) -> Result<String> {
    let mut seed = manifest.clone();
    seed.id = "manifest:pending".to_string();
    stable_typed_id("manifest", &seed).map_err(SyncError::from)
}

fn validate_manifest_record(
    path: &Path,
    expected_id: &str,
    manifest: &ManifestRecord,
) -> Result<()> {
    validate_manifest_id(path, "manifest", expected_id)?;
    if manifest.id != expected_id {
        return Err(SyncError::InvalidManifest {
            path: path.to_path_buf(),
            message: format!(
                "manifest body id `{}` does not match expected `{expected_id}`",
                manifest.id
            ),
        });
    }
    if manifest.version != MANIFEST_VERSION {
        return Err(SyncError::InvalidManifest {
            path: path.to_path_buf(),
            message: format!(
                "unsupported manifest version {}; expected {MANIFEST_VERSION}",
                manifest.version
            ),
        });
    }
    let actual = expected_manifest_id(manifest)?;
    if actual != expected_id {
        return Err(SyncError::InvalidManifest {
            path: path.to_path_buf(),
            message: format!(
                "manifest `{expected_id}` does not match canonical body id `{actual}`"
            ),
        });
    }
    Ok(())
}

fn validate_latest_manifest(
    repo: &gix::Repository,
    remote_public: &Path,
) -> Result<Option<ManifestRecord>> {
    let Some(manifest_id) = read_manifest_id(remote_public)? else {
        if remote_public.exists() && fs::read_dir(remote_public)?.next().transpose()?.is_some() {
            return Err(SyncError::InvalidManifest {
                path: remote_public.join("manifest").join("HEAD"),
                message: "remote public store has files but no manifest HEAD".to_string(),
            });
        }
        return Ok(None);
    };
    let manifest_path = manifest_path(remote_public, &manifest_id);
    let manifest = read_manifest_record(&manifest_path)?;
    validate_manifest_record(&manifest_path, &manifest_id, &manifest)?;
    validate_manifest_chain(repo, remote_public, &manifest_id)?;
    validate_partition_tip(
        repo,
        &manifest_path,
        PublicPartition::Facts,
        &manifest.facts_tip,
    )?;
    validate_partition_tip(
        repo,
        &manifest_path,
        PublicPartition::Blobs,
        &manifest.blobs_tip,
    )?;
    Ok(Some(manifest))
}

fn validate_manifest_chain(
    repo: &gix::Repository,
    remote_public: &Path,
    head_id: &str,
) -> Result<()> {
    collect_manifest_chain_ids(repo, remote_public, head_id).map(|_| ())
}

fn collect_manifest_chain_ids(
    repo: &gix::Repository,
    remote_public: &Path,
    head_id: &str,
) -> Result<BTreeSet<String>> {
    let mut seen = BTreeSet::new();
    let mut cursor = Some(head_id.to_string());
    while let Some(manifest_id) = cursor {
        let path = manifest_path(remote_public, &manifest_id);
        if !seen.insert(manifest_id.clone()) {
            return Err(SyncError::InvalidManifest {
                path,
                message: format!("manifest prev chain contains a cycle at `{manifest_id}`"),
            });
        }
        let manifest = read_manifest_record(&path)?;
        validate_manifest_record(&path, &manifest_id, &manifest)?;
        validate_partition_object(repo, &path, PublicPartition::Facts, &manifest.facts_tip)?;
        validate_partition_object(repo, &path, PublicPartition::Blobs, &manifest.blobs_tip)?;
        cursor = manifest.prev_manifest.clone();
        if let Some(prev) = &cursor {
            validate_manifest_id(&path, "prev_manifest", prev)?;
            let prev_path = manifest_path(remote_public, prev);
            if !prev_path.exists() {
                return Err(SyncError::InvalidManifest {
                    path,
                    message: format!(
                        "prev_manifest `{prev}` is missing from remote manifest store"
                    ),
                });
            }
        }
    }
    Ok(seen)
}

fn validate_partition_tip(
    repo: &gix::Repository,
    manifest_path: &Path,
    partition: PublicPartition,
    tip: &str,
) -> Result<()> {
    validate_partition_object(repo, manifest_path, partition, tip)?;
    let mut reference =
        repo.find_reference(partition.ref_name())
            .map_err(|error| SyncError::InvalidManifest {
                path: manifest_path.to_path_buf(),
                message: format!("missing {}: {error}", partition.ref_name()),
            })?;
    let ref_id = reference
        .peel_to_id()
        .map_err(|error| SyncError::InvalidManifest {
            path: manifest_path.to_path_buf(),
            message: format!(
                "{} does not resolve to an object id: {error}",
                partition.ref_name()
            ),
        })?;
    if ref_id.to_string() != tip {
        return Err(SyncError::InvalidManifest {
            path: manifest_path.to_path_buf(),
            message: format!(
                "{} points to {}, but manifest {}_tip is {tip}",
                partition.ref_name(),
                ref_id,
                partition.label()
            ),
        });
    }
    Ok(())
}

fn validate_partition_object(
    repo: &gix::Repository,
    manifest_path: &Path,
    partition: PublicPartition,
    tip: &str,
) -> Result<()> {
    let tip_id = parse_git_object_id(manifest_path, partition.label(), tip)?;
    match repo
        .try_find_object(tip_id)
        .map_err(|error| SyncError::Gix(error.to_string()))?
    {
        Some(_) => {}
        None => {
            return Err(SyncError::InvalidManifest {
                path: manifest_path.to_path_buf(),
                message: format!(
                    "{}_tip `{tip}` does not exist in remote object database",
                    partition.label()
                ),
            });
        }
    }
    Ok(())
}

fn parse_git_object_id(path: &Path, field: &str, value: &str) -> Result<gix::ObjectId> {
    use std::str::FromStr;

    gix::ObjectId::from_str(value).map_err(|error| SyncError::InvalidManifest {
        path: path.to_path_buf(),
        message: format!("{field}_tip is not a valid Git object id `{value}`: {error}"),
    })
}

fn parse_manifest_head(path: &Path, value: &str) -> Result<String> {
    let id = value.trim();
    validate_manifest_id(path, "manifest HEAD", id)?;
    Ok(id.to_string())
}

fn validate_manifest_id(path: &Path, field: &str, id: &str) -> Result<()> {
    let Some(digest) = id.strip_prefix("manifest:") else {
        return Err(SyncError::InvalidManifestHead {
            path: path.to_path_buf(),
            message: format!("{field} expected manifest:<digest>"),
        });
    };
    if digest.len() != 12 || !digest.bytes().all(is_lower_hex_digit) {
        return Err(SyncError::InvalidManifestHead {
            path: path.to_path_buf(),
            message: format!("{field} digest must be 12 lowercase hex characters"),
        });
    }
    Ok(())
}

fn is_lower_hex_digit(byte: u8) -> bool {
    byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)
}

fn update_ref(repo: &gix::Repository, ref_name: &str, target: String) -> Result<()> {
    use std::str::FromStr;

    let target = gix::ObjectId::from_str(&target).map_err(|err| SyncError::Gix(err.to_string()))?;
    repo.reference(
        ref_name,
        target,
        gix::refs::transaction::PreviousValue::Any,
        format!("graft sync update {ref_name}"),
    )
    .map_err(|err| SyncError::Gix(err.to_string()))?;
    Ok(())
}

fn digest_public_tree(root: &Path) -> Result<String> {
    digest_tree_filtered(root, |_| true)
}

fn digest_public_partition(root: &Path, partition: PublicPartition) -> Result<String> {
    match partition {
        PublicPartition::Facts => digest_tree_filtered(root, is_public_fact_path),
        PublicPartition::Blobs => digest_public_tree(&root.join("blob")),
    }
}

fn digest_tree_filtered<F>(root: &Path, include: F) -> Result<String>
where
    F: Copy + Fn(&Path) -> bool,
{
    let mut rows = Vec::new();
    collect_digest_rows(root, root, &mut rows, include)?;
    rows.sort();
    Ok(blake3_hex_digest(rows.join("\n").as_bytes()))
}

fn collect_digest_rows<F>(
    root: &Path,
    path: &Path,
    rows: &mut Vec<String>,
    include: F,
) -> Result<()>
where
    F: Copy + Fn(&Path) -> bool,
{
    if !path.exists() {
        return Ok(());
    }
    let mut children = Vec::new();
    for entry in fs::read_dir(path)? {
        children.push(entry?);
    }
    children.sort_by_key(|entry| entry.path());
    for entry in children {
        let path = entry.path();
        let file_type = entry.file_type()?;
        let relative = path
            .strip_prefix(root)
            .map_err(|_| SyncError::InvalidStorePath {
                path: path.to_path_buf(),
                message: format!("path is not under digest root {}", root.display()),
            })?;
        if !include(relative) {
            continue;
        }
        if file_type.is_dir() {
            collect_digest_rows(root, &path, rows, include)?;
        } else if file_type.is_file() {
            let rel = digest_relative_path(root, &path)?;
            rows.push(format!("{} {}", rel, blake3_hex_digest(&fs::read(&path)?)));
        }
    }
    Ok(())
}

fn digest_relative_path(root: &Path, path: &Path) -> Result<String> {
    let relative = path
        .strip_prefix(root)
        .map_err(|_| SyncError::InvalidStorePath {
            path: path.to_path_buf(),
            message: format!("path is not under digest root {}", root.display()),
        })?;
    let mut parts = Vec::new();
    for component in relative.components() {
        match component {
            Component::Normal(value) => {
                let value = value.to_str().ok_or_else(|| SyncError::InvalidStorePath {
                    path: path.to_path_buf(),
                    message: "public store paths must be valid UTF-8".to_string(),
                })?;
                parts.push(value.to_string());
            }
            Component::CurDir => {}
            other => {
                return Err(SyncError::InvalidStorePath {
                    path: path.to_path_buf(),
                    message: format!(
                        "unexpected relative path component {}",
                        other.as_os_str().to_string_lossy()
                    ),
                });
            }
        }
    }
    if parts.is_empty() {
        return Err(SyncError::InvalidStorePath {
            path: path.to_path_buf(),
            message: "file path did not produce a relative store path".to_string(),
        });
    }
    Ok(parts.join("/"))
}

fn count_files(root: &Path) -> Result<usize> {
    count_files_filtered(root, |_| true)
}

fn count_public_facts_files(root: &Path) -> Result<usize> {
    count_files_filtered(root, is_public_fact_path)
}

fn count_files_filtered<F>(root: &Path, include: F) -> Result<usize>
where
    F: Copy + Fn(&Path) -> bool,
{
    if !root.exists() {
        return Ok(0);
    }
    let mut count = 0;
    for entry in fs::read_dir(root)? {
        let entry = entry?;
        let path = entry.path();
        let relative = path
            .strip_prefix(root)
            .map_err(|_| SyncError::InvalidStorePath {
                path: path.to_path_buf(),
                message: format!("path is not under count root {}", root.display()),
            })?;
        if !include(relative) {
            continue;
        }
        if entry.file_type()?.is_dir() {
            count += count_files_filtered(&path, include)?;
        } else if entry.file_type()?.is_file() {
            count += 1;
        }
    }
    Ok(count)
}

fn is_public_fact_path(relative: &Path) -> bool {
    !matches!(
        relative.components().next(),
        Some(Component::Normal(value)) if value == "blob" || value == "manifest"
    )
}

fn validate_public_store_objects(public: &Path) -> Result<()> {
    if !public.exists() {
        return Ok(());
    }
    for entry in sorted_dir_entries(public)? {
        let path = entry.path();
        let file_type = entry.file_type()?;
        let name = path_file_name(&path, "public store root entry")?;
        if !file_type.is_dir() {
            return Err(SyncError::InvalidStoreObject {
                path,
                message: "public store root only accepts typed object directories".to_string(),
            });
        }
        match name.as_str() {
            "blob" => validate_blob_dir(&path)?,
            "tree" => validate_typed_json_dir::<TreeSnapshot, _>(&path, "tree", |path, value| {
                value
                    .id()
                    .map_err(SyncError::from)
                    .map_err(|error| SyncError::InvalidStoreObject {
                        path: path.to_path_buf(),
                        message: error.to_string(),
                    })
            })?,
            "action" => validate_typed_json_dir::<Action, _>(&path, "action", |path, value| {
                action_id(value).map(|id| id.to_string()).map_err(|error| {
                    SyncError::InvalidStoreObject {
                        path: path.to_path_buf(),
                        message: error.to_string(),
                    }
                })
            })?,
            "application" => validate_typed_json_dir::<ApplicationRecord, _>(
                &path,
                "application",
                |path, value| {
                    application_id(value)
                        .map(|id| id.to_string())
                        .map_err(|error| SyncError::InvalidStoreObject {
                            path: path.to_path_buf(),
                            message: error.to_string(),
                        })
                },
            )?,
            "change" => validate_typed_json_dir::<Change, _>(&path, "change", |path, value| {
                value
                    .id()
                    .map(|id| id.to_string())
                    .map_err(|error| SyncError::InvalidStoreObject {
                        path: path.to_path_buf(),
                        message: error.to_string(),
                    })
            })?,
            "property" => validate_property_dir(&path)?,
            "patch" => validate_typed_json_dir::<PatchRecord, _>(&path, "patch", |path, value| {
                let actual = patch_id(value).map(|id| id.to_string()).map_err(|error| {
                    SyncError::InvalidStoreObject {
                        path: path.to_path_buf(),
                        message: error.to_string(),
                    }
                })?;
                validate_embedded_id(path, "patch", value.id.to_string(), &actual)?;
                Ok(actual)
            })?,
            "evidence_refs" => validate_evidence_refs_dir(&path)?,
            "relation" => {
                validate_typed_json_dir::<PatchRelation, _>(&path, "relation", |path, value| {
                    let actual = relation_id(value)
                        .map(|id| id.to_string())
                        .map_err(|error| SyncError::InvalidStoreObject {
                            path: path.to_path_buf(),
                            message: error.to_string(),
                        })?;
                    validate_embedded_id(path, "relation", value.id.to_string(), &actual)?;
                    Ok(actual)
                })?
            }
            "promotion" => {
                validate_typed_json_dir::<PromotionRecord, _>(&path, "promotion", |path, value| {
                    let actual = promotion_id(value)
                        .map(|id| id.to_string())
                        .map_err(|error| SyncError::InvalidStoreObject {
                            path: path.to_path_buf(),
                            message: error.to_string(),
                        })?;
                    validate_embedded_id(path, "promotion", value.id.to_string(), &actual)?;
                    Ok(actual)
                })?
            }
            "manifest" => validate_manifest_sidecar_dir(&path)?,
            _ => {
                return Err(SyncError::InvalidStoreObject {
                    path,
                    message: format!("unknown public store object directory `{name}`"),
                });
            }
        }
    }
    validate_public_application_graph(public)?;
    Ok(())
}

fn validate_public_application_graph(public: &Path) -> Result<()> {
    let actions = read_public_object_map::<Action>(public, "action")?;
    let changes = read_public_object_map::<Change>(public, "change")?;
    let applications = read_public_object_map::<ApplicationRecord>(public, "application")?;
    for (application_id, application) in applications {
        let path = public
            .join("application")
            .join(format!("{application_id}.json"));
        let action = actions.get(application.action.as_str()).ok_or_else(|| {
            SyncError::InvalidStoreObject {
                path: path.clone(),
                message: format!(
                    "application `{application_id}` references missing action `{}`",
                    application.action
                ),
            }
        })?;
        let change = changes.get(application.change.as_str()).ok_or_else(|| {
            SyncError::InvalidStoreObject {
                path: path.clone(),
                message: format!(
                    "application `{application_id}` references missing change `{}`",
                    application.change
                ),
            }
        })?;
        validate_application_integrity(&application, action, change).map_err(|error| {
            SyncError::InvalidStoreObject {
                path,
                message: error.to_string(),
            }
        })?;
    }
    Ok(())
}

fn read_public_object_map<T: DeserializeOwned>(
    public: &Path,
    kind: &'static str,
) -> Result<BTreeMap<String, T>> {
    let dir = public.join(kind);
    let mut objects = BTreeMap::new();
    for entry in sorted_dir_entries(&dir)? {
        let path = entry.path();
        if !entry.file_type()?.is_file() {
            continue;
        }
        let id = json_object_id(&path, kind)?;
        let object = read_store_json::<T>(&path, kind)?;
        objects.insert(id, object);
    }
    Ok(objects)
}

fn validate_property_dir(dir: &Path) -> Result<()> {
    for entry in sorted_dir_entries(dir)? {
        let path = entry.path();
        let file_type = entry.file_type()?;
        if !file_type.is_file() {
            return Err(SyncError::InvalidStoreObject {
                path,
                message: "property store only accepts flat .json objects".to_string(),
            });
        }
        let expected = json_object_id(&path, "property")?;
        let spec = read_store_json::<PropertySpec>(&path, "property")?;
        let actual = spec
            .property_id()
            .map(|id| id.to_string())
            .map_err(|error| SyncError::InvalidStoreObject {
                path: path.clone(),
                message: error.to_string(),
            })?;
        if actual != expected {
            return Err(SyncError::InvalidStoreObject {
                path,
                message: format!(
                    "property object filename `{expected}` does not match canonical id `{actual}`"
                ),
            });
        }
    }
    Ok(())
}

fn validate_blob_dir(dir: &Path) -> Result<()> {
    for entry in sorted_dir_entries(dir)? {
        let path = entry.path();
        let file_type = entry.file_type()?;
        if !file_type.is_file() {
            return Err(SyncError::InvalidStoreObject {
                path,
                message: "blob store only accepts flat content-addressed files".to_string(),
            });
        }
        let expected = path_file_name(&path, "blob store object")?;
        let actual = blake3_hex_digest(&fs::read(&path)?);
        if actual != expected {
            return Err(SyncError::InvalidStoreObject {
                path,
                message: format!(
                    "blob filename `{expected}` does not match content hash `{actual}`"
                ),
            });
        }
    }
    Ok(())
}

fn validate_typed_json_dir<T, F>(dir: &Path, kind: &'static str, expected_id: F) -> Result<()>
where
    T: DeserializeOwned,
    F: Copy + Fn(&Path, &T) -> Result<String>,
{
    for entry in sorted_dir_entries(dir)? {
        let path = entry.path();
        let file_type = entry.file_type()?;
        if !file_type.is_file() {
            return Err(SyncError::InvalidStoreObject {
                path,
                message: format!("{kind} store only accepts flat .json objects"),
            });
        }
        let expected = json_object_id(&path, kind)?;
        let value = read_store_json::<T>(&path, kind)?;
        let actual = expected_id(&path, &value)?;
        if actual != expected {
            return Err(SyncError::InvalidStoreObject {
                path,
                message: format!(
                    "{kind} object filename `{expected}` does not match canonical id `{actual}`"
                ),
            });
        }
    }
    Ok(())
}

fn validate_evidence_refs_dir(dir: &Path) -> Result<()> {
    for entry in sorted_dir_entries(dir)? {
        let path = entry.path();
        if !entry.file_type()?.is_file() {
            return Err(SyncError::InvalidStoreObject {
                path,
                message: "evidence_refs store only accepts flat .json objects".to_string(),
            });
        }
        json_object_id(&path, "evidence_refs")?;
        read_evidence_refs_file(&path)?;
    }
    Ok(())
}

fn validate_manifest_sidecar_dir(dir: &Path) -> Result<()> {
    for entry in sorted_dir_entries(dir)? {
        let path = entry.path();
        if !entry.file_type()?.is_file() {
            return Err(SyncError::InvalidStoreObject {
                path,
                message: "manifest store only accepts flat files".to_string(),
            });
        }
        let name = path_file_name(&path, "manifest store object")?;
        if name == "HEAD" {
            parse_manifest_head(&path, &fs::read_to_string(&path)?)?;
            continue;
        }
        let expected = json_object_id(&path, "manifest")?;
        let manifest = read_store_json::<ManifestRecord>(&path, "manifest")?;
        validate_manifest_record(&path, &expected, &manifest).map_err(|error| {
            SyncError::InvalidStoreObject {
                path,
                message: error.to_string(),
            }
        })?;
    }
    Ok(())
}

fn validate_embedded_id(path: &Path, kind: &str, embedded: String, actual: &str) -> Result<()> {
    if embedded != actual {
        return Err(SyncError::InvalidStoreObject {
            path: path.to_path_buf(),
            message: format!("{kind} body id `{embedded}` does not match canonical id `{actual}`"),
        });
    }
    Ok(())
}

fn read_store_json<T: DeserializeOwned>(path: &Path, kind: &str) -> Result<T> {
    serde_json::from_slice(&fs::read(path)?).map_err(|error| SyncError::InvalidStoreObject {
        path: path.to_path_buf(),
        message: format!("invalid {kind} JSON: {error}"),
    })
}

fn json_object_id(path: &Path, kind: &str) -> Result<String> {
    if path.extension().and_then(|value| value.to_str()) != Some("json") {
        return Err(SyncError::InvalidStoreObject {
            path: path.to_path_buf(),
            message: format!("{kind} store object must use a .json extension"),
        });
    }
    path.file_stem()
        .and_then(|value| value.to_str())
        .filter(|value| !value.is_empty())
        .map(ToString::to_string)
        .ok_or_else(|| SyncError::InvalidStoreObject {
            path: path.to_path_buf(),
            message: format!("{kind} store object name must be valid UTF-8"),
        })
}

fn path_file_name(path: &Path, role: &str) -> Result<String> {
    path.file_name()
        .and_then(|value| value.to_str())
        .filter(|value| !value.is_empty())
        .map(ToString::to_string)
        .ok_or_else(|| SyncError::InvalidStoreObject {
            path: path.to_path_buf(),
            message: format!("{role} name must be valid UTF-8"),
        })
}

fn sorted_dir_entries(dir: &Path) -> Result<Vec<fs::DirEntry>> {
    if !dir.exists() {
        return Ok(Vec::new());
    }
    let mut entries = Vec::new();
    for entry in fs::read_dir(dir)? {
        entries.push(entry?);
    }
    entries.sort_by_key(|entry| entry.path());
    Ok(entries)
}

fn copy_public_tree(from: &Path, to: &Path, union_evidence_refs: bool) -> Result<usize> {
    validate_public_tree_copy_compatible(from, from, to, union_evidence_refs)?;
    copy_public_tree_inner(from, from, to, union_evidence_refs)
}

fn validate_public_tree_copy_compatible(
    root: &Path,
    from: &Path,
    to: &Path,
    union_evidence_refs: bool,
) -> Result<()> {
    if !from.exists() {
        return Ok(());
    }
    for entry in sorted_dir_entries(from)? {
        let source = entry.path();
        let dest = to.join(entry.file_name());
        let file_type = entry.file_type()?;
        if file_type.is_dir() {
            validate_public_tree_copy_compatible(root, &source, &dest, union_evidence_refs)?;
        } else if file_type.is_file() && dest.exists() {
            let relative = source
                .strip_prefix(root)
                .map_err(|_| SyncError::InvalidStorePath {
                    path: source.clone(),
                    message: format!("path is not under copy root {}", root.display()),
                })?;
            if union_evidence_refs && is_evidence_refs_file(relative) {
                continue;
            }
            if is_mutable_manifest_head(relative) {
                continue;
            }
            if fs::read(&source)? != fs::read(&dest)? {
                return Err(SyncError::InvalidStoreObject {
                    path: dest,
                    message: format!(
                        "destination already has different bytes for immutable public object `{}`",
                        digest_relative_path(root, &source)?
                    ),
                });
            }
        }
    }
    Ok(())
}

fn copy_public_tree_inner(
    root: &Path,
    from: &Path,
    to: &Path,
    union_evidence_refs: bool,
) -> Result<usize> {
    if !from.exists() {
        return Ok(0);
    }
    let mut changed = 0;
    for entry in sorted_dir_entries(from)? {
        let source = entry.path();
        let dest = to.join(entry.file_name());
        let file_type = entry.file_type()?;
        if file_type.is_dir() {
            changed += copy_public_tree_inner(root, &source, &dest, union_evidence_refs)?;
        } else if file_type.is_file() {
            fs::create_dir_all(to)?;
            let relative = source
                .strip_prefix(root)
                .map_err(|_| SyncError::InvalidStorePath {
                    path: source.clone(),
                    message: format!("path is not under copy root {}", root.display()),
                })?;
            let is_evidence_refs =
                union_evidence_refs && is_evidence_refs_file(relative) && dest.exists();
            if is_evidence_refs {
                if merge_evidence_refs_file(&source, &dest)? {
                    changed += 1;
                }
            } else if !dest.exists() {
                fs::copy(&source, &dest)?;
                changed += 1;
            } else {
                let source_bytes = fs::read(&source)?;
                let dest_bytes = fs::read(&dest)?;
                if source_bytes != dest_bytes {
                    if is_mutable_manifest_head(relative) {
                        fs::copy(&source, &dest)?;
                        changed += 1;
                    } else {
                        return Err(SyncError::InvalidStoreObject {
                            path: dest,
                            message: format!(
                                "destination already has different bytes for immutable public object `{}`",
                                digest_relative_path(root, &source)?
                            ),
                        });
                    }
                }
            }
        }
    }
    Ok(changed)
}

fn is_evidence_refs_file(relative: &Path) -> bool {
    matches!(
        relative.components().next(),
        Some(Component::Normal(value)) if value == "evidence_refs"
    )
}

fn is_mutable_manifest_head(relative: &Path) -> bool {
    let mut components = relative.components();
    matches!(
        (components.next(), components.next(), components.next()),
        (
            Some(Component::Normal(dir)),
            Some(Component::Normal(file)),
            None
        ) if dir == "manifest" && file == "HEAD"
    )
}

fn merge_evidence_refs_file(source: &Path, dest: &Path) -> Result<bool> {
    let src = read_evidence_refs_file(source)?;
    let mut dst = read_evidence_refs_file(dest)?;
    if src.owner != dst.owner {
        return Err(SyncError::InvalidEvidenceRefs {
            path: source.to_path_buf(),
            message: format!(
                "source owner `{}` does not match destination owner `{}`",
                src.owner, dst.owner
            ),
        });
    }

    let mut changed = false;
    for value in src.evidence {
        if !dst.evidence.contains(&value) {
            dst.evidence.push(value);
            changed = true;
        }
    }
    let updated_at = newest_updated_at(dst.updated_at.as_deref(), src.updated_at.as_deref());
    if dst.updated_at != updated_at {
        dst.updated_at = updated_at;
        changed = true;
    }
    if changed {
        fs::write(dest, serde_json::to_vec_pretty(&dst)?)?;
    }
    Ok(changed)
}

fn read_evidence_refs_file(path: &Path) -> Result<EvidenceRefs> {
    let owner = evidence_refs_owner(path)?;
    let value: serde_json::Value = serde_json::from_slice(&fs::read(path)?)?;
    if value.is_object() {
        let refs: EvidenceRefs = serde_json::from_value(value)?;
        if refs.owner != owner {
            return Err(SyncError::InvalidEvidenceRefs {
                path: path.to_path_buf(),
                message: format!("owner `{}` does not match file owner `{owner}`", refs.owner),
            });
        }
        return Ok(refs);
    }
    Err(SyncError::InvalidEvidenceRefs {
        path: path.to_path_buf(),
        message: "expected evidence refs object with owner and evidence fields".to_string(),
    })
}

fn evidence_refs_owner(path: &Path) -> Result<String> {
    path.file_stem()
        .and_then(|value| value.to_str())
        .filter(|value| !value.is_empty())
        .map(ToString::to_string)
        .ok_or_else(|| SyncError::InvalidEvidenceRefs {
            path: path.to_path_buf(),
            message: "could not infer owner from file name".to_string(),
        })
}

fn newest_updated_at(left: Option<&str>, right: Option<&str>) -> Option<String> {
    match (left, right) {
        (Some(left), Some(right)) => Some(left.max(right).to_string()),
        (Some(value), None) | (None, Some(value)) => Some(value.to_string()),
        (None, None) => None,
    }
}

#[cfg(test)]
mod tests {
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
        let manifest =
            read_manifest_record(&manifest_path(&remote_public, &second_manifest)).unwrap();

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
}
