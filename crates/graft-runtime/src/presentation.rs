use anyhow::Result;
use graft_core::{
    ApplicationRef, EvidenceRecord, EvidenceResult, GraftCandidate, PatchRecord, PromotionRecord,
    StateId,
};
use graft_explain::NextAction;
use graft_store::GraftStore;

use crate::requirements::{constraint_primitives, property_label};
use crate::resolved_application;
use crate::view::{
    CandidateSummary, ChangeView, EvidenceCounts, EvidenceView, PatchSummary, PromotionView,
};

pub(crate) fn summarize_candidate(
    store: &GraftStore,
    candidate: &GraftCandidate,
) -> Result<CandidateSummary> {
    let evidence = store.cached_evidence_for_subject(candidate.id.as_str())?;
    summarize_candidate_with_evidence(store, candidate, &evidence)
}

pub(crate) fn summarize_candidate_with_evidence(
    store: &GraftStore,
    candidate: &GraftCandidate,
    evidence: &[EvidenceRecord],
) -> Result<CandidateSummary> {
    let resolved = resolved_application(store, &candidate.application)?;
    Ok(CandidateSummary {
        id: candidate.id.to_string(),
        base_state: state_label(&resolved.record.base_state),
        target_state: state_label(&resolved.record.target_state),
        constraint: constraint_primitives(&candidate.constraint)
            .iter()
            .map(property_label)
            .collect(),
        producer: candidate.provenance.producer.clone(),
        message: candidate.provenance.message.clone(),
        created_at: candidate.provenance.created_at.clone(),
        evidence: EvidenceCounts::from_records(evidence),
        change: change_view_for_application(store, &candidate.application)?,
    })
}

pub(crate) fn summarize_patch_with_evidence(
    store: &GraftStore,
    patch: &PatchRecord,
    evidence: &[EvidenceRecord],
) -> Result<PatchSummary> {
    let resolved = resolved_application(store, &patch.application)?;
    Ok(PatchSummary {
        id: patch.id.to_string(),
        base_state: state_label(&resolved.record.base_state),
        target_state: state_label(&resolved.record.target_state),
        constraint: constraint_primitives(&patch.constraint)
            .iter()
            .map(property_label)
            .collect(),
        producer: patch.provenance.producer.clone(),
        message: patch.provenance.message.clone(),
        admitted_at: patch.provenance.created_at.clone(),
        evidence: EvidenceCounts::from_records(evidence),
        change: change_view_for_application(store, &patch.application)?,
    })
}

pub(crate) fn promotion_view(promotion: &PromotionRecord) -> PromotionView {
    PromotionView {
        id: promotion.id.to_string(),
        patch_id: promotion.patch_id.to_string(),
        target: promotion.target.clone(),
        dry_run: promotion.dry_run,
        status: promotion.status.clone(),
        promoted_at: promotion.promoted_at.clone(),
    }
}

pub(crate) fn change_view_for_application(
    store: &GraftStore,
    application: &ApplicationRef,
) -> Result<Option<ChangeView>> {
    let resolved = resolved_application(store, application)?;
    let summary = resolved.change.summary();
    Ok(Some(ChangeView {
        id: Some(resolved.record.change.to_string()),
        description: None,
        files: summary.files,
        added: summary.added,
        modified: summary.modified,
        deleted: summary.deleted,
        unchanged: summary.unchanged,
        captured: summary.captured,
        target_bytes: summary.target_bytes,
        sample_paths: resolved
            .change
            .changed_paths()
            .into_iter()
            .take(8)
            .collect(),
    }))
}

pub(crate) fn evidence_view(record: &EvidenceRecord) -> EvidenceView {
    EvidenceView {
        id: record.id.to_string(),
        subject: record.subject.clone(),
        property: record.property.to_string(),
        verifier: record.verifier.clone(),
        result: result_label(&record.result),
        created_at: record.created_at.clone(),
    }
}

fn result_label(result: &EvidenceResult) -> String {
    match result {
        EvidenceResult::Passed => "passed".to_string(),
        EvidenceResult::Failed { reason } => format!("failed: {reason}"),
        EvidenceResult::Unknown { reason } => format!("unknown: {reason}"),
        EvidenceResult::Skipped { reason } => format!("skipped: {reason}"),
    }
}

pub(crate) fn state_label(state: &StateId) -> String {
    match state {
        StateId::GitTree(value) => format!("git-tree:{value}"),
        StateId::RepoTree(repo) => repo.display_ref(),
        StateId::GraftTree(value) => format!("graft-tree:{value}"),
    }
}

pub(crate) fn next_search_actions(patch: &PatchRecord) -> Vec<NextAction> {
    next_actions_for_patch(patch, false, false)
}

pub(crate) fn next_actions_for_patch(
    patch: &PatchRecord,
    materialized: bool,
    promoted: bool,
) -> Vec<NextAction> {
    let ctx = graft_explain::next_actions::PatchContext {
        id: patch.id.to_string(),
        constraint_primitives: constraint_primitives(&patch.constraint)
            .iter()
            .map(property_label)
            .collect(),
        materialized,
        promoted,
    };
    graft_explain::next_actions::next_actions_patch(&ctx)
}

pub(crate) fn next_actions_for_candidate(
    candidate: &GraftCandidate,
    evidence: &[EvidenceRecord],
) -> Vec<NextAction> {
    let counts = EvidenceCounts::from_records(evidence);
    let ctx = graft_explain::next_actions::CandidateContext {
        id: candidate.id.to_string(),
        passed: counts.passed,
        failed: counts.failed,
        unknown: counts.unknown,
        skipped: counts.skipped,
        constraint_primitives: constraint_primitives(&candidate.constraint)
            .iter()
            .map(property_label)
            .collect(),
    };
    graft_explain::next_actions::next_actions(&ctx)
}
