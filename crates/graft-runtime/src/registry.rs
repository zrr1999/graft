use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Result, bail};
use clap::Subcommand;
use graft_core::{
    Action, ApplicationRecord, Change, EvidenceRecord, GraftCandidate, PatchRecord, PatchRelation,
    PromotionRecord, TreeSnapshot, candidate_id, patch_id,
};
use graft_store::{EvidenceRefsRecord, GraftStore};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

use crate::view::CommandEnvelope;

#[derive(Subcommand, Debug)]
pub(crate) enum RegistryCommand {
    /// Export admitted public objects as a JSON bundle to the given path
    Export {
        /// Output path for the JSON registry bundle
        path: PathBuf,
    },
    /// Import a previously exported registry JSON bundle into public store
    Import {
        /// Rewrite legacy v1 patch properties/admitted_at fields into v2 constraints while importing
        #[arg(long)]
        upgrade_from_v1: bool,
        /// Input path of the JSON registry bundle
        path: PathBuf,
    },
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct RegistryBundle {
    #[serde(default)]
    trees: Vec<TreeObject>,
    #[serde(default)]
    actions: Vec<ActionObject>,
    #[serde(default)]
    changes: Vec<ChangeObject>,
    #[serde(default)]
    applications: Vec<ApplicationObject>,
    #[serde(default)]
    blobs: Vec<BlobObject>,
    #[serde(default)]
    evidence_refs: Vec<EvidenceRefsRecord>,
    #[serde(default)]
    candidates: Vec<GraftCandidate>,
    patches: Vec<PatchRecord>,
    evidence: Vec<EvidenceRecord>,
    relations: Vec<PatchRelation>,
    promotions: Vec<PromotionRecord>,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct TreeObject {
    id: String,
    snapshot: TreeSnapshot,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct ChangeObject {
    id: String,
    change: Change,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct ActionObject {
    id: String,
    action: Action,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct ApplicationObject {
    id: String,
    application: ApplicationRecord,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct BlobObject {
    hash: String,
    bytes: Vec<u8>,
}

pub(crate) fn run_registry_command(
    store: &GraftStore,
    cwd: &Path,
    command: &RegistryCommand,
) -> Result<CommandEnvelope> {
    match command {
        RegistryCommand::Export { path } => {
            let bundle = RegistryBundle {
                trees: store
                    .list_tree_objects()?
                    .into_iter()
                    .map(|(id, snapshot)| TreeObject { id, snapshot })
                    .collect(),
                actions: store
                    .list_action_objects()?
                    .into_iter()
                    .map(|(id, action)| ActionObject { id, action })
                    .collect(),
                changes: store
                    .list_change_objects()?
                    .into_iter()
                    .map(|(id, change)| ChangeObject { id, change })
                    .collect(),
                applications: store
                    .list_application_objects()?
                    .into_iter()
                    .map(|(id, application)| ApplicationObject { id, application })
                    .collect(),
                blobs: store
                    .list_blob_objects()?
                    .into_iter()
                    .map(|(hash, bytes)| BlobObject { hash, bytes })
                    .collect(),
                patches: store.list_patches()?,
                evidence_refs: store.list_patch_evidence_refs()?,
                candidates: Vec::new(),
                evidence: store.list_registry_evidence()?,
                relations: store.list_relations()?,
                promotions: store.list_promotions()?,
            };
            let path = resolve_path(cwd, path);
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent)?;
            }
            fs::write(&path, serde_json::to_vec_pretty(&bundle)?)?;
            Ok(CommandEnvelope {
                message: Some(format!("exported registry to {}", path.display())),
                ..CommandEnvelope::ok()
            })
        }
        RegistryCommand::Import {
            path,
            upgrade_from_v1,
        } => {
            store.init_storage()?;
            let path = resolve_path(cwd, path);
            let bytes = fs::read(&path)?;
            let value: Value = serde_json::from_slice(&bytes)?;
            let value = if *upgrade_from_v1 {
                upgrade_bundle_from_v1(value)?
            } else {
                reject_legacy_bundle(&value)?;
                value
            };
            let mut bundle: RegistryBundle = serde_json::from_value(value)?;
            if *upgrade_from_v1 {
                recompute_upgraded_ids(&mut bundle)?;
            }
            for blob in &bundle.blobs {
                store.write_blob_object(&blob.hash, &blob.bytes)?;
            }
            for tree in &bundle.trees {
                let (id, _) = store.write_tree_snapshot(&tree.snapshot)?;
                if id != tree.id {
                    bail!(
                        "{}",
                        graft_explain::diagnostics::m001_registry_tree_id_mismatch(&tree.id)
                            .format_reason()
                    );
                }
            }
            for action in &bundle.actions {
                let (id, _) = store.write_action(&action.action)?;
                if id.as_str() != action.id {
                    bail!(
                        "[E_REGISTRY_BUNDLE] action id mismatch: expected {}, got {}",
                        action.id,
                        id
                    );
                }
            }
            for change in &bundle.changes {
                let (id, _) = store.write_change(&change.change)?;
                if id.as_str() != change.id {
                    bail!(
                        "{}",
                        graft_explain::diagnostics::m002_registry_change_id_mismatch(&change.id)
                            .format_reason()
                    );
                }
            }
            for application in &bundle.applications {
                let (id, _) = store.write_application(&application.application)?;
                if id.as_str() != application.id {
                    bail!(
                        "[E_REGISTRY_BUNDLE] application id mismatch: expected {}, got {}",
                        application.id,
                        id
                    );
                }
            }
            for candidate in &bundle.candidates {
                store.write_candidate(candidate)?;
            }
            for patch in &bundle.patches {
                store.write_patch(patch)?;
            }
            for evidence in &bundle.evidence {
                store.write_registry_evidence(evidence)?;
            }
            let imported_patch_ids = bundle
                .patches
                .iter()
                .map(|patch| patch.id.to_string())
                .collect::<BTreeSet<_>>();
            for refs in &bundle.evidence_refs {
                if !imported_patch_ids.contains(&refs.owner) {
                    bail!(
                        "[E_REGISTRY_BUNDLE] evidence_refs owner {} is not present in bundle patches",
                        refs.owner
                    );
                }
                store.write_patch_evidence_refs(refs)?;
            }
            for relation in &bundle.relations {
                store.write_relation(relation)?;
            }
            for promotion in &bundle.promotions {
                store.write_promotion(promotion)?;
            }
            Ok(CommandEnvelope {
                message: Some(format!("imported registry from {}", path.display())),
                patch_ids: bundle
                    .patches
                    .iter()
                    .map(|patch| patch.id.to_string())
                    .collect(),
                registry_changed: true,
                ..CommandEnvelope::ok()
            })
        }
    }
}

fn reject_legacy_bundle(value: &Value) -> Result<()> {
    if let Some(candidates) = value.get("candidates").and_then(Value::as_array) {
        for candidate in candidates {
            let Some(object) = candidate.as_object() else {
                continue;
            };
            if object.contains_key("expected") {
                bail!(
                    "[E_UNSUPPORTED_STORE_SCHEMA] registry bundle contains v1 candidate field expected; rerun with `graft bundle import --upgrade-from-v1 <path>` to rewrite legacy expected into v2 constraints"
                );
            }
        }
    }
    let Some(patches) = value.get("patches").and_then(Value::as_array) else {
        return Ok(());
    };
    for patch in patches {
        let Some(object) = patch.as_object() else {
            continue;
        };
        let legacy_fields = ["properties", "admitted_at"]
            .into_iter()
            .filter(|field| object.contains_key(*field))
            .collect::<Vec<_>>();
        if !legacy_fields.is_empty() {
            bail!(
                "[E_UNSUPPORTED_STORE_SCHEMA] registry bundle contains v1 patch fields {}; rerun with `graft bundle import --upgrade-from-v1 <path>` to rewrite legacy properties/admitted_at into v2 constraints",
                legacy_fields.join(", ")
            );
        }
    }
    Ok(())
}

fn upgrade_bundle_from_v1(mut value: Value) -> Result<Value> {
    if let Some(candidates) = value.get_mut("candidates").and_then(Value::as_array_mut) {
        for candidate in candidates {
            let Some(object) = candidate.as_object_mut() else {
                continue;
            };
            upgrade_candidate_from_v1(object)?;
        }
    }
    if let Some(patches) = value.get_mut("patches").and_then(Value::as_array_mut) {
        for patch in patches {
            let Some(object) = patch.as_object_mut() else {
                continue;
            };
            upgrade_patch_from_v1(object)?;
        }
    }
    Ok(value)
}

fn upgrade_candidate_from_v1(object: &mut Map<String, Value>) -> Result<()> {
    let expected = object.remove("expected");
    if !object.contains_key("constraint") {
        object.insert(
            "constraint".to_string(),
            constraint_from_legacy_expected(expected.as_ref())?,
        );
    }
    Ok(())
}

fn upgrade_patch_from_v1(object: &mut Map<String, Value>) -> Result<()> {
    let properties = object.remove("properties");
    object.remove("admitted_at");
    if !object.contains_key("constraint") {
        object.insert(
            "constraint".to_string(),
            constraint_from_legacy_properties(properties.as_ref())?,
        );
    }
    if !object.contains_key("admission") {
        let constraint = object
            .get("constraint")
            .cloned()
            .unwrap_or_else(|| json!({"kind": "top"}));
        object.insert("admission".to_string(), json!({ "constraint": constraint }));
    }
    Ok(())
}

fn constraint_from_legacy_expected(expected: Option<&Value>) -> Result<Value> {
    let Some(expected) = expected else {
        return Ok(json!({ "kind": "top" }));
    };
    let expected = expected.as_array().ok_or_else(|| {
        anyhow::anyhow!(
            "[E_UNSUPPORTED_STORE_SCHEMA] legacy candidate expected must be an array when using --upgrade-from-v1"
        )
    })?;
    let primitives = expected
        .iter()
        .map(|property| json!({ "kind": "primitive", "property": property }))
        .collect::<Vec<_>>();
    Ok(all_of_json(primitives))
}

fn recompute_upgraded_ids(bundle: &mut RegistryBundle) -> Result<()> {
    let mut patch_ids = BTreeMap::new();
    let mut candidate_ids = BTreeMap::new();
    for candidate in &mut bundle.candidates {
        let old = candidate.id.to_string();
        candidate.id = candidate_id(candidate)?;
        candidate_ids.insert(old, candidate.id.to_string());
    }
    for patch in &mut bundle.patches {
        let old = patch.id.to_string();
        patch.id = patch_id(patch)?;
        patch_ids.insert(old, patch.id.to_string());
    }
    for refs in &mut bundle.evidence_refs {
        if let Some(new_id) = patch_ids.get(&refs.owner) {
            refs.owner.clone_from(new_id);
        }
    }
    for relation in &mut bundle.relations {
        if let Some(new_id) = patch_ids
            .get(&relation.subject)
            .or_else(|| candidate_ids.get(&relation.subject))
        {
            relation.subject.clone_from(new_id);
        }
        for source in &mut relation.sources {
            if let Some(new_id) = patch_ids.get(source).or_else(|| candidate_ids.get(source)) {
                source.clone_from(new_id);
            }
        }
    }
    for promotion in &mut bundle.promotions {
        if let Some(new_id) = patch_ids.get(promotion.patch_id.as_str()) {
            promotion.patch_id = graft_core::PatchId::new(new_id.clone());
        }
    }
    Ok(())
}

fn constraint_from_legacy_properties(properties: Option<&Value>) -> Result<Value> {
    let Some(properties) = properties else {
        return Ok(json!({ "kind": "top" }));
    };
    let properties = properties.as_array().ok_or_else(|| {
        anyhow::anyhow!(
            "[E_UNSUPPORTED_STORE_SCHEMA] legacy patch properties must be an array when using --upgrade-from-v1"
        )
    })?;
    let primitives = properties
        .iter()
        .map(|property| json!({ "kind": "primitive", "property": property }))
        .collect::<Vec<_>>();
    Ok(all_of_json(primitives))
}

fn all_of_json(mut items: Vec<Value>) -> Value {
    match items.len() {
        0 => json!({ "kind": "top" }),
        1 => items.pop().unwrap(),
        _ => {
            let first = items.remove(0);
            json!({
                "kind": "both",
                "left": first,
                "right": all_of_json(items),
            })
        }
    }
}

fn resolve_path(cwd: &Path, path: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        cwd.join(path)
    }
}
