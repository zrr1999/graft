use std::path::Path;

use anyhow::{Context, Result, bail};
use graft_core::PatchRecord;
use graft_explain::NextAction;
use graft_promote::GixBackend;
use graft_store::GraftStore;

use crate::config::GraftConfig;
use crate::repo::materialized_snapshot_for_state;
use crate::state_label;

pub(crate) fn target_snapshot_for_patch(
    store: &GraftStore,
    config: &GraftConfig,
    patch: &PatchRecord,
) -> Result<graft_core::TreeSnapshot> {
    let target_state = store
        .resolve_application(&patch.application)?
        .record
        .target_state;
    materialized_snapshot_for_state(store, config, &target_state).with_context(|| {
        format!(
            "materialize patch {} target state {}",
            patch.id,
            state_label(&target_state)
        )
    })
}

pub(crate) fn ensure_materialized_commit(
    git: &GixBackend,
    store: &GraftStore,
    config: &GraftConfig,
    cwd: &Path,
    patch: &PatchRecord,
    id: &str,
) -> Result<String> {
    let graft_ref = materialize_ref_name(id, None);
    match git.try_resolve_ref(cwd, &graft_ref)? {
        Some(commit_id) => Ok(commit_id),
        None => {
            let snapshot = target_snapshot_for_patch(store, config, patch)?;
            Ok(git
                .materialize_commit(
                    cwd,
                    &snapshot,
                    store.paths().object_blobs(),
                    &format!("graft patch promote {id}"),
                    Some(&graft_ref),
                )?
                .commit_id)
        }
    }
}

pub(crate) fn promote_next_action(
    id: &str,
    to: &str,
    pr: bool,
    release: Option<&str>,
) -> NextAction {
    let label = if pr {
        format!("graft patch promote {id} --to {to} --pr --yes")
    } else if let Some(tag) = release {
        format!("graft patch promote {id} --to {to} --release {tag} --yes")
    } else {
        format!("graft patch promote {id} --to {to} --yes")
    };
    NextAction::new(
        "promote.apply",
        label,
        graft_explain::NextActionKind::Dangerous,
        "applying the promotion will mutate a real Git ref / PR / release",
    )
}

pub(crate) fn materialize_ref_name(patch_id: &str, requested: Option<&str>) -> String {
    match requested {
        Some(name) if name.starts_with("refs/") => name.to_string(),
        Some(name) => format!(
            "refs/graft/patches/{}",
            git_ref_component_for_patch_id(name)
        ),
        None => format!(
            "refs/graft/patches/{}",
            git_ref_component_for_patch_id(patch_id)
        ),
    }
}

pub(crate) fn git_ref_component_for_patch_id(id: &str) -> &str {
    id.strip_prefix("patch:").unwrap_or(id)
}

pub(crate) fn validate_promote_ref_args(
    to: &str,
    branch: Option<&str>,
    release: Option<&str>,
    head: Option<&str>,
) -> Result<()> {
    validate_optional_cli_ref_arg("--to", Some(to))?;
    validate_optional_cli_ref_arg("--branch", branch)?;
    validate_optional_cli_ref_arg("--release", release)?;
    validate_optional_cli_ref_arg("--head", head)?;
    Ok(())
}

fn validate_optional_cli_ref_arg(label: &str, value: Option<&str>) -> Result<()> {
    if value.is_some_and(|value| value.trim().is_empty()) {
        bail!("{label} must not be empty");
    }
    Ok(())
}
