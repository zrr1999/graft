use std::collections::BTreeSet;
use std::fs;
use std::path::{Component, Path, PathBuf};

use graft_core::{blake3_hex_digest, stable_typed_id};
use serde::{Deserialize, Serialize};

use crate::{Result, SyncError};

const MANIFEST_VERSION: u32 = 2;

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ManifestRecord {
    pub id: String,
    pub version: u32,
    pub facts_tip: String,
    pub blobs_tip: String,
    pub prev_manifest: Option<String>,
    pub summary: ManifestSummary,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ManifestSummary {
    pub facts_files: usize,
    pub blob_files: usize,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum PublicPartition {
    Facts,
    Blobs,
}

impl PublicPartition {
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Facts => "facts",
            Self::Blobs => "blobs",
        }
    }

    pub(crate) fn ref_name(self) -> &'static str {
        match self {
            Self::Facts => "refs/graft/facts",
            Self::Blobs => "refs/graft/blobs",
        }
    }
}

pub(crate) fn write_partition_ref(
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

pub(crate) fn write_manifest(
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

pub(crate) fn write_manifest_sidecar(public: &Path, manifest: &ManifestRecord) -> Result<()> {
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

pub(crate) fn read_manifest_id(remote_public: &Path) -> Result<Option<String>> {
    let path = remote_public.join("manifest").join("HEAD");
    match fs::read_to_string(&path) {
        Ok(value) => parse_manifest_head(&path, &value).map(Some),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}

pub(crate) fn read_valid_previous_manifest_id(
    repo: &gix::Repository,
    remote_public: &Path,
) -> Result<Option<String>> {
    let Some(manifest_id) = read_manifest_id(remote_public)? else {
        return Ok(None);
    };
    validate_manifest_chain(repo, remote_public, &manifest_id)?;
    Ok(Some(manifest_id))
}

pub(crate) fn manifest_path(remote_public: &Path, manifest_id: &str) -> PathBuf {
    remote_public
        .join("manifest")
        .join(format!("{manifest_id}.json"))
}

pub(crate) fn read_manifest_record(path: &Path) -> Result<ManifestRecord> {
    serde_json::from_slice(&fs::read(path)?).map_err(|error| SyncError::InvalidManifest {
        path: path.to_path_buf(),
        message: error.to_string(),
    })
}

pub(crate) fn expected_manifest_id(manifest: &ManifestRecord) -> Result<String> {
    let mut seed = manifest.clone();
    seed.id = "manifest:pending".to_string();
    stable_typed_id("manifest", &seed).map_err(SyncError::from)
}

pub(crate) fn validate_manifest_record(
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

pub(crate) fn validate_latest_manifest(
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

pub(crate) fn validate_manifest_chain(
    repo: &gix::Repository,
    remote_public: &Path,
    head_id: &str,
) -> Result<()> {
    collect_manifest_chain_ids(repo, remote_public, head_id).map(|_| ())
}

pub(crate) fn collect_manifest_chain_ids(
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

pub(crate) fn validate_partition_tip(
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

pub(crate) fn validate_partition_object(
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

pub(crate) fn parse_git_object_id(path: &Path, field: &str, value: &str) -> Result<gix::ObjectId> {
    use std::str::FromStr;

    gix::ObjectId::from_str(value).map_err(|error| SyncError::InvalidManifest {
        path: path.to_path_buf(),
        message: format!("{field}_tip is not a valid Git object id `{value}`: {error}"),
    })
}

pub(crate) fn parse_manifest_head(path: &Path, value: &str) -> Result<String> {
    let id = value.trim();
    validate_manifest_id(path, "manifest HEAD", id)?;
    Ok(id.to_string())
}

pub(crate) fn validate_manifest_id(path: &Path, field: &str, id: &str) -> Result<()> {
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

pub(crate) fn digest_public_partition(root: &Path, partition: PublicPartition) -> Result<String> {
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

pub(crate) fn digest_relative_path(root: &Path, path: &Path) -> Result<String> {
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

pub(crate) fn count_files(root: &Path) -> Result<usize> {
    count_files_filtered(root, |_| true)
}

pub(crate) fn count_public_facts_files(root: &Path) -> Result<usize> {
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
