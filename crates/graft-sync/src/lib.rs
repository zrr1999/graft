use std::fs;
use std::path::Path;

use graft_core::{blake3_hex_digest, stable_typed_id};
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
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ManifestRecord {
    pub id: String,
    pub version: u32,
    pub created_at: String,
    pub by: String,
    pub facts_tip: String,
    pub blobs_tip: String,
    pub prev_manifest: Option<String>,
    pub summary: ManifestSummary,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct ManifestSummary {
    pub facts_files: usize,
    pub blob_files: usize,
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
}

pub fn sync_public_store(
    workspace_root: &Path,
    remote: &Path,
    push: bool,
    fetch: bool,
) -> Result<SyncReport> {
    let remote_repo = ensure_storage_repo(remote)?;
    let local_public = workspace_root.join("store").join("public");
    let remote_public = remote.join("graft-public");
    fs::create_dir_all(&local_public)?;
    fs::create_dir_all(&remote_public)?;

    let mut report = SyncReport::default();
    if push {
        report.pushed += copy_public_tree(&local_public, &remote_public, true)?;
        report.facts_tip = Some(write_partition_ref(
            &remote_repo,
            &remote_public,
            "facts",
            "refs/graft/facts",
        )?);
        report.blobs_tip = Some(write_partition_ref(
            &remote_repo,
            &remote_public.join("blob"),
            "blobs",
            "refs/graft/blobs",
        )?);
        let manifest = write_manifest(
            &remote_repo,
            &remote_public,
            report.facts_tip.clone().unwrap_or_default(),
            report.blobs_tip.clone().unwrap_or_default(),
        )?;
        report.manifest_id = Some(manifest.id);
    }
    if fetch {
        report.fetched += copy_public_tree(&remote_public, &local_public, true)?;
    }
    Ok(report)
}

fn ensure_storage_repo(remote: &Path) -> Result<gix::Repository> {
    fs::create_dir_all(remote)?;
    match gix::open(remote) {
        Ok(repo) => Ok(repo),
        Err(_) => gix::init_bare(remote).map_err(|err| SyncError::Gix(err.to_string())),
    }
}

fn write_partition_ref(
    repo: &gix::Repository,
    source: &Path,
    label: &str,
    ref_name: &str,
) -> Result<String> {
    let digest = digest_public_tree(source)?;
    let body = serde_json::json!({
        "version": 1,
        "partition": label,
        "digest": digest,
        "written_at": time::OffsetDateTime::now_utc().to_string(),
    });
    let bytes = serde_json::to_vec_pretty(&body)?;
    let blob = repo
        .write_blob(bytes)
        .map_err(|err| SyncError::Gix(err.to_string()))?;
    update_ref(repo, ref_name, blob.to_string())?;
    Ok(blob.to_string())
}

fn write_manifest(
    repo: &gix::Repository,
    remote_public: &Path,
    facts_tip: String,
    blobs_tip: String,
) -> Result<ManifestRecord> {
    let summary = ManifestSummary {
        facts_files: count_files(remote_public)?,
        blob_files: count_files(&remote_public.join("blob"))?,
    };
    let mut manifest = ManifestRecord {
        id: "manifest:pending".to_string(),
        version: 1,
        created_at: time::OffsetDateTime::now_utc().to_string(),
        by: "graft-sync".to_string(),
        facts_tip,
        blobs_tip,
        prev_manifest: read_manifest_id(remote_public)?,
        summary,
    };
    manifest.id = stable_typed_id("manifest", &manifest)?;
    fs::create_dir_all(remote_public.join("manifest"))?;
    fs::write(
        remote_public
            .join("manifest")
            .join(format!("{}.json", manifest.id)),
        serde_json::to_vec_pretty(&manifest)?,
    )?;
    fs::write(remote_public.join("manifest").join("HEAD"), &manifest.id)?;
    let blob = repo
        .write_blob(serde_json::to_vec_pretty(&manifest)?)
        .map_err(|err| SyncError::Gix(err.to_string()))?;
    update_ref(repo, "refs/graft/manifests", blob.to_string())?;
    Ok(manifest)
}

fn read_manifest_id(remote_public: &Path) -> Result<Option<String>> {
    let path = remote_public.join("manifest").join("HEAD");
    match fs::read_to_string(path) {
        Ok(value) => Ok(Some(value.trim().to_string()).filter(|value| !value.is_empty())),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
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
    let mut rows = Vec::new();
    collect_digest_rows(root, root, &mut rows)?;
    rows.sort();
    Ok(blake3_hex_digest(rows.join("\n").as_bytes()))
}

fn collect_digest_rows(root: &Path, path: &Path, rows: &mut Vec<String>) -> Result<()> {
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
        if file_type.is_dir() {
            collect_digest_rows(root, &path, rows)?;
        } else if file_type.is_file() {
            let rel = path.strip_prefix(root).unwrap_or(&path).to_string_lossy();
            rows.push(format!("{} {}", rel, blake3_hex_digest(&fs::read(&path)?)));
        }
    }
    Ok(())
}

fn count_files(root: &Path) -> Result<usize> {
    if !root.exists() {
        return Ok(0);
    }
    let mut count = 0;
    for entry in fs::read_dir(root)? {
        let entry = entry?;
        if entry.file_type()?.is_dir() {
            count += count_files(&entry.path())?;
        } else if entry.file_type()?.is_file() {
            count += 1;
        }
    }
    Ok(count)
}

fn copy_public_tree(from: &Path, to: &Path, union_evidence_refs: bool) -> Result<usize> {
    if !from.exists() {
        return Ok(0);
    }
    let mut changed = 0;
    for entry in fs::read_dir(from)? {
        let entry = entry?;
        let source = entry.path();
        let dest = to.join(entry.file_name());
        let file_type = entry.file_type()?;
        if file_type.is_dir() {
            changed += copy_public_tree(&source, &dest, union_evidence_refs)?;
        } else if file_type.is_file() {
            fs::create_dir_all(to)?;
            let is_evidence_refs = union_evidence_refs
                && source
                    .parent()
                    .and_then(Path::file_name)
                    .is_some_and(|name| name == "evidence_refs")
                && dest.exists();
            if is_evidence_refs {
                if merge_evidence_refs_file(&source, &dest)? {
                    changed += 1;
                }
            } else if !dest.exists() || fs::read(&source)? != fs::read(&dest)? {
                fs::copy(&source, &dest)?;
                changed += 1;
            }
        }
    }
    Ok(changed)
}

fn merge_evidence_refs_file(source: &Path, dest: &Path) -> Result<bool> {
    let src: serde_json::Value = serde_json::from_slice(&fs::read(source)?)?;
    let mut dst: serde_json::Value = serde_json::from_slice(&fs::read(dest)?)?;
    let src_evidence = src
        .get("evidence")
        .and_then(serde_json::Value::as_array)
        .cloned()
        .unwrap_or_default();
    let Some(dst_evidence) = dst
        .get_mut("evidence")
        .and_then(serde_json::Value::as_array_mut)
    else {
        fs::copy(source, dest)?;
        return Ok(true);
    };
    let mut changed = false;
    for value in src_evidence {
        if !dst_evidence.contains(&value) {
            dst_evidence.push(value);
            changed = true;
        }
    }
    if changed {
        if let Some(updated_at) = dst.get_mut("updated_at") {
            *updated_at = serde_json::Value::String(time::OffsetDateTime::now_utc().to_string());
        }
        fs::write(dest, serde_json::to_vec_pretty(&dst)?)?;
    }
    Ok(changed)
}
