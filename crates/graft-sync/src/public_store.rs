use std::collections::BTreeMap;
use std::fs;
use std::path::{Component, Path};

use graft_core::{
    Action, ApplicationRecord, Change, ConstraintDef, PatchRecord, PatchRelation, Plan,
    PromotionRecord, TreeSnapshot, action_id, application_id, blake3_hex_digest, patch_id,
    promotion_id, relation_id, validate_application_integrity,
};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use crate::manifest::{
    ManifestRecord, digest_relative_path, parse_manifest_head, validate_manifest_record,
};
use crate::{Result, SyncError};

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct EvidenceRefs {
    pub(crate) owner: String,
    pub(crate) evidence: Vec<String>,
    pub(crate) updated_at: Option<String>,
}

pub(crate) fn validate_public_store_objects(public: &Path) -> Result<()> {
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
            "constraint" => validate_constraint_dir(&path)?,
            "plan" => validate_typed_json_dir::<Plan, _>(&path, "plan", |path, value| {
                value.plan_id().map(|id| id.to_string()).map_err(|error| {
                    SyncError::InvalidStoreObject {
                        path: path.to_path_buf(),
                        message: error.to_string(),
                    }
                })
            })?,
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

fn validate_constraint_dir(dir: &Path) -> Result<()> {
    for entry in sorted_dir_entries(dir)? {
        let path = entry.path();
        let file_type = entry.file_type()?;
        if !file_type.is_file() {
            return Err(SyncError::InvalidStoreObject {
                path,
                message: "constraint store only accepts flat .json objects".to_string(),
            });
        }
        let expected = json_object_id(&path, "constraint")?;
        let def = read_store_json::<ConstraintDef>(&path, "constraint")?;
        let actual = def
            .body_id()
            .map_err(|error| SyncError::InvalidStoreObject {
                path: path.clone(),
                message: error.to_string(),
            })?;
        if actual != expected {
            return Err(SyncError::InvalidStoreObject {
                path,
                message: format!(
                    "constraint object filename `{expected}` does not match canonical id `{actual}`"
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

pub(crate) fn read_store_json<T: DeserializeOwned>(path: &Path, kind: &str) -> Result<T> {
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

pub(crate) fn copy_public_tree(from: &Path, to: &Path, union_evidence_refs: bool) -> Result<usize> {
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

pub(crate) fn copy_public_tree_inner(
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

pub(crate) fn merge_evidence_refs_file(source: &Path, dest: &Path) -> Result<bool> {
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

pub(crate) fn read_evidence_refs_file(path: &Path) -> Result<EvidenceRefs> {
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
