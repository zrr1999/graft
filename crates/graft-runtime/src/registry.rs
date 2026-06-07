use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Result, bail};
use clap::Subcommand;
use graft_core::{
    Action, ApplicationRecord, Change, EvidenceRecord, PatchRecord, PatchRelation, PromotionRecord,
    TreeSnapshot,
};
use graft_store::{EvidenceRefsRecord, GraftStore};
use serde::{Deserialize, Serialize};

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
        RegistryCommand::Import { path } => {
            store.init_storage()?;
            let path = resolve_path(cwd, path);
            let bytes = fs::read(&path)?;
            let bundle: RegistryBundle = serde_json::from_slice(&bytes)?;
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

fn resolve_path(cwd: &Path, path: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        cwd.join(path)
    }
}
