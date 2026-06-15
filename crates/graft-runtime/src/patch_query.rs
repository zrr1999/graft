use anyhow::{Result, bail};
use graft_core::{EvidenceResult, PlanId, StateId};
use graft_store::GraftStore;

use crate::config::{GraftConfig, load_graft_config};
use crate::presentation::{
    state_label, summarize_candidate_with_evidence, summarize_patch_with_evidence,
};
use crate::requirements::{
    constraint_matches_request, constraint_primitives, plan_label, resolve_constraint_ref,
};
use crate::view::{CandidateSummary, CommandEnvelope, PatchSummary};

pub(crate) fn run_patch_list_command(
    store: &GraftStore,
    candidates: bool,
    all: bool,
    constraint: &Option<String>,
    producer: &Option<String>,
) -> Result<CommandEnvelope> {
    if candidates && all {
        bail!("patch list cannot use --candidates and --all together");
    }
    let patches = if candidates {
        Vec::new()
    } else {
        list_patch_summaries(store, constraint, producer)?
    };
    let candidate_summaries = if candidates || all {
        list_candidate_summaries(store, constraint, false, producer)?
    } else {
        Vec::new()
    };
    let message = if all {
        format!(
            "listed {} admitted patch(es) and {} candidate(s)",
            patches.len(),
            candidate_summaries.len()
        )
    } else if candidates {
        format!("listed {} candidate(s)", candidate_summaries.len())
    } else {
        format!("listed {} admitted patch(es)", patches.len())
    };
    Ok(CommandEnvelope {
        message: Some(message),
        patches,
        candidates: candidate_summaries,
        ..CommandEnvelope::ok()
    })
}

fn list_patch_summaries(
    store: &GraftStore,
    constraint: &Option<String>,
    producer: &Option<String>,
) -> Result<Vec<PatchSummary>> {
    let mut patches = store.list_patches()?;
    if let Some(constraint) = constraint {
        let config = load_graft_config(store)?;
        warn_if_constraint_unknown(constraint, &config);
        let mut filtered = Vec::new();
        for patch in patches {
            let mut matched = false;
            for expr in constraint_primitives(&patch.constraint) {
                if constraint_matches_request(&config, &expr, constraint)? {
                    matched = true;
                    break;
                }
            }
            if matched {
                filtered.push(patch);
            }
        }
        patches = filtered;
    }
    if let Some(producer) = producer {
        patches.retain(|patch| patch.provenance.producer == *producer);
    }
    patches.sort_by(|left, right| left.id.cmp(&right.id));
    patches
        .iter()
        .map(|patch| {
            let evidence = store.registry_evidence_for_subject(patch.id.as_str())?;
            summarize_patch_with_evidence(store, patch, &evidence)
        })
        .collect()
}

pub(crate) fn list_candidate_summaries(
    store: &GraftStore,
    constraint: &Option<String>,
    failed: bool,
    producer: &Option<String>,
) -> Result<Vec<CandidateSummary>> {
    let constraint_filter = match constraint.as_deref() {
        Some(constraint) => {
            let config = load_graft_config(store)?;
            warn_if_constraint_unknown(constraint, &config);
            Some((constraint, config))
        }
        None => None,
    };
    let mut summaries = Vec::new();
    for candidate in store.list_candidates()? {
        if let Some((constraint, config)) = constraint_filter.as_ref() {
            let mut matched = false;
            for expr in constraint_primitives(&candidate.constraint) {
                if constraint_matches_request(config, &expr, constraint)? {
                    matched = true;
                    break;
                }
            }
            if !matched {
                continue;
            }
        }
        if let Some(producer) = producer
            && candidate.provenance.producer != *producer
        {
            continue;
        }
        let evidence = store.cached_evidence_for_subject(candidate.id.as_str())?;
        if failed
            && !evidence
                .iter()
                .any(|record| matches!(&record.result, EvidenceResult::Failed { .. }))
        {
            continue;
        }
        summaries.push(summarize_candidate_with_evidence(
            store, &candidate, &evidence,
        )?);
    }
    summaries.sort_by(|left, right| left.id.cmp(&right.id));
    Ok(summaries)
}

pub(crate) fn warn_if_constraint_unknown(name: &str, config: &GraftConfig) {
    if config.constraints.contains_key(name) {
        return;
    }
    eprintln!("warning: constraint `{name}` is not declared in constraints.roto");
    eprintln!("hint:    run `graft constraint list` for configured constraints");
}

pub(crate) fn constraint_id_matches(
    config: &GraftConfig,
    constraint: &PlanId,
    requested: &str,
) -> Result<bool> {
    constraint_matches_request(config, constraint, requested)
}

pub(crate) fn incoming_command(store: &GraftStore) -> Result<CommandEnvelope> {
    let mut patches = store.list_patches()?;
    patches.sort_by_key(|patch| {
        let base_state = store
            .resolve_application(&patch.application)
            .map(|resolved| resolved.record.base_state)
            .unwrap_or(StateId::GraftTree("application:missing".to_string()));
        (
            state_label(&base_state),
            patch.provenance.created_at.clone(),
            patch.id.to_string(),
        )
    });
    let mut lines = Vec::new();
    let mut current_base: Option<String> = None;
    for patch in &patches {
        let base = state_label(
            &store
                .resolve_application(&patch.application)?
                .record
                .base_state,
        );
        if current_base.as_deref() != Some(base.as_str()) {
            current_base = Some(base.clone());
            lines.push(format!("base {base}"));
        }
        let evidence_refs = store.patch_evidence_index(patch.id.as_str())?;
        let local_evidence = evidence_refs
            .iter()
            .filter(|id| {
                store
                    .paths()
                    .object_evidence()
                    .join(format!("{id}.json"))
                    .exists()
            })
            .count();
        let local_status = if evidence_refs.is_empty() {
            "no evidence_refs".to_string()
        } else if local_evidence == evidence_refs.len() {
            format!("locally rebuilt {local_evidence}/{}", evidence_refs.len())
        } else {
            format!(
                "not locally rebuilt {local_evidence}/{}",
                evidence_refs.len()
            )
        };
        let patch_constraints = constraint_primitives(&patch.constraint);
        let constraints = if patch_constraints.is_empty() {
            "(no constraints)".to_string()
        } else {
            patch_constraints
                .iter()
                .map(plan_label)
                .collect::<Vec<_>>()
                .join(", ")
        };
        lines.push(format!(
            "  - {} [{}] {}",
            patch.id, constraints, local_status
        ));
    }
    if lines.is_empty() {
        lines.push("no incoming patches".to_string());
    }
    let summaries = patches
        .iter()
        .map(|patch| summarize_patch_with_evidence(store, patch, &[]))
        .collect::<Result<Vec<_>>>()?;
    Ok(CommandEnvelope {
        message: Some(lines.join(
            "
",
        )),
        patches: summaries,
        ..CommandEnvelope::ok()
    })
}

pub(crate) fn search_patches(
    store: &GraftStore,
    constraint: &Option<String>,
    base: &Option<String>,
    producer: &Option<String>,
    has_evidence: &Option<String>,
) -> Result<Vec<String>> {
    let mut patches = store.list_patches()?;
    if let Some(constraint) = constraint {
        let config = load_graft_config(store)?;
        let mut filtered = Vec::new();
        for patch in patches {
            let mut matched = false;
            for expr in constraint_primitives(&patch.constraint) {
                if constraint_matches_request(&config, &expr, constraint)? {
                    matched = true;
                    break;
                }
            }
            if matched {
                filtered.push(patch);
            }
        }
        patches = filtered;
    }
    if let Some(base) = base {
        patches.retain(|patch| {
            store
                .resolve_application(&patch.application)
                .ok()
                .is_some_and(|resolved| state_label(&resolved.record.base_state).contains(base))
        });
    }
    if let Some(producer) = producer {
        patches.retain(|patch| patch.provenance.producer == *producer);
    }
    if let Some(constraint) = has_evidence {
        let config = load_graft_config(store)?;
        let constraint = resolve_constraint_ref(&config, constraint)?;
        let mut filtered = Vec::new();
        for patch in patches {
            let evidence = store.registry_evidence_for_subject(patch.id.as_str())?;
            let mut matched = false;
            for record in &evidence {
                if record.subject == patch.id.as_str()
                    && record.plan == constraint
                    && record.result.satisfies_requirement()
                {
                    matched = true;
                    break;
                }
            }
            if matched {
                filtered.push(patch);
            }
        }
        patches = filtered;
    }
    patches.sort_by(|left, right| left.id.cmp(&right.id));
    Ok(patches
        .into_iter()
        .map(|patch| patch.id.to_string())
        .collect())
}
