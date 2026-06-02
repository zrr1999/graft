use graft_core::EvidenceRecord;
use graft_explain::NextAction;
use serde::{Deserialize, Serialize};

#[derive(Debug, Deserialize, Serialize)]
pub(crate) struct CommandEnvelope {
    pub(crate) status: String,
    pub(crate) message: Option<String>,
    pub(crate) candidate_id: Option<String>,
    pub(crate) patch_id: Option<String>,
    pub(crate) evidence_ids: Vec<String>,
    pub(crate) patch_ids: Vec<String>,
    pub(crate) candidates: Vec<CandidateSummary>,
    pub(crate) patches: Vec<PatchSummary>,
    pub(crate) evidence: Vec<EvidenceView>,
    pub(crate) change: Option<ChangeView>,
    pub(crate) promotions: Vec<PromotionView>,
    pub(crate) cache_changed: bool,
    pub(crate) registry_changed: bool,
    pub(crate) git_changed: bool,
    pub(crate) next_actions: Vec<NextAction>,
}

impl CommandEnvelope {
    pub(crate) fn ok() -> Self {
        Self {
            status: "ok".to_string(),
            message: None,
            candidate_id: None,
            patch_id: None,
            evidence_ids: Vec::new(),
            patch_ids: Vec::new(),
            candidates: Vec::new(),
            patches: Vec::new(),
            evidence: Vec::new(),
            change: None,
            promotions: Vec::new(),
            cache_changed: false,
            registry_changed: false,
            git_changed: false,
            next_actions: Vec::new(),
        }
    }
}

#[derive(Debug, Deserialize, Serialize)]
pub(crate) struct CandidateSummary {
    pub(crate) id: String,
    pub(crate) base_state: String,
    pub(crate) target_state: String,
    pub(crate) expected: Vec<String>,
    pub(crate) producer: String,
    pub(crate) message: Option<String>,
    pub(crate) created_at: String,
    pub(crate) evidence: EvidenceCounts,
    pub(crate) change: Option<ChangeView>,
}

#[derive(Debug, Deserialize, Serialize)]
pub(crate) struct PatchSummary {
    pub(crate) id: String,
    pub(crate) base_state: String,
    pub(crate) target_state: String,
    pub(crate) properties: Vec<String>,
    pub(crate) producer: String,
    pub(crate) message: Option<String>,
    pub(crate) admitted_at: String,
    pub(crate) evidence: EvidenceCounts,
    pub(crate) change: Option<ChangeView>,
}

#[derive(Debug, Deserialize, Serialize)]
pub(crate) struct PromotionView {
    pub(crate) id: String,
    pub(crate) patch_id: String,
    pub(crate) target: String,
    pub(crate) dry_run: bool,
    pub(crate) status: String,
    pub(crate) promoted_at: String,
}

#[derive(Debug, Deserialize, Serialize)]
pub(crate) struct EvidenceView {
    pub(crate) id: String,
    pub(crate) subject: String,
    pub(crate) property: String,
    pub(crate) verifier: String,
    pub(crate) result: String,
    pub(crate) created_at: String,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub(crate) struct EvidenceCounts {
    pub(crate) total: usize,
    pub(crate) passed: usize,
    pub(crate) failed: usize,
    pub(crate) unknown: usize,
    pub(crate) skipped: usize,
}

impl EvidenceCounts {
    pub(crate) fn from_records(records: &[EvidenceRecord]) -> Self {
        let mut counts = Self {
            total: records.len(),
            ..Self::default()
        };
        for record in records {
            match &record.result {
                graft_core::EvidenceResult::Passed => counts.passed += 1,
                graft_core::EvidenceResult::Failed { .. } => counts.failed += 1,
                graft_core::EvidenceResult::Unknown { .. } => counts.unknown += 1,
                graft_core::EvidenceResult::Skipped { .. } => counts.skipped += 1,
            }
        }
        counts
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub(crate) struct ChangeView {
    pub(crate) id: Option<String>,
    pub(crate) description: Option<String>,
    pub(crate) files: usize,
    pub(crate) added: usize,
    pub(crate) modified: usize,
    pub(crate) deleted: usize,
    pub(crate) unchanged: usize,
    pub(crate) captured: usize,
    pub(crate) target_bytes: u64,
    pub(crate) sample_paths: Vec<String>,
}

pub(crate) fn print_human(envelope: &CommandEnvelope) {
    println!("status: {}", envelope.status);
    if let Some(message) = &envelope.message {
        println!("{message}");
    }
    if let Some(candidate_id) = &envelope.candidate_id {
        println!("candidate: {candidate_id}");
    }
    if let Some(patch_id) = &envelope.patch_id {
        println!("patch: {patch_id}");
    }
    for candidate in &envelope.candidates {
        print_candidate(candidate);
    }
    for patch in &envelope.patches {
        print_patch(patch);
    }
    for promotion in &envelope.promotions {
        print_promotion(promotion);
    }
    for patch_id in &envelope.patch_ids {
        println!("patch: {patch_id}");
    }
    if let Some(change) = &envelope.change {
        print_change(change);
    }
    for evidence in &envelope.evidence {
        println!(
            "evidence: {} {} {} ({})",
            evidence.id, evidence.property, evidence.result, evidence.verifier
        );
    }
    println!("cache {}", changed_word(envelope.cache_changed));
    println!("registry {}", changed_word(envelope.registry_changed));
    println!("git {}", changed_word(envelope.git_changed));
    print_hole_report(&envelope.next_actions);
}

fn print_hole_report(actions: &[NextAction]) {
    if actions.is_empty() {
        return;
    }
    println!("next:");
    for action in actions {
        println!("  {} {}", action.kind.label(), action.label);
        println!("      {}", action.why);
    }
}

fn print_candidate(candidate: &CandidateSummary) {
    println!("candidate: {}", candidate.id);
    println!("  base: {}", candidate.base_state);
    println!("  target: {}", candidate.target_state);
    println!("  expected: {}", joined_or_dash(&candidate.expected));
    println!(
        "  evidence: {} passed, {} failed, {} unknown, {} skipped",
        candidate.evidence.passed,
        candidate.evidence.failed,
        candidate.evidence.unknown,
        candidate.evidence.skipped
    );
    if let Some(change) = &candidate.change {
        print_change(change);
    }
}

fn print_patch(patch: &PatchSummary) {
    println!("patch: {}", patch.id);
    println!("  base: {}", patch.base_state);
    println!("  target: {}", patch.target_state);
    println!("  properties: {}", joined_or_dash(&patch.properties));
    println!("  admitted_at: {}", patch.admitted_at);
    println!(
        "  evidence: {} passed, {} failed, {} unknown, {} skipped",
        patch.evidence.passed,
        patch.evidence.failed,
        patch.evidence.unknown,
        patch.evidence.skipped
    );
    if let Some(change) = &patch.change {
        print_change(change);
    }
}

fn print_promotion(promotion: &PromotionView) {
    println!("promotion: {}", promotion.id);
    println!("  patch: {}", promotion.patch_id);
    println!("  target: {}", promotion.target);
    println!("  status: {}", promotion.status);
    println!("  dry_run: {}", promotion.dry_run);
}

fn print_change(change: &ChangeView) {
    if let Some(id) = &change.id {
        println!("change: {id}");
    }
    if let Some(description) = &change.description {
        println!("change: {description}");
    }
    println!(
        "  files: {} added, {} modified, {} deleted, {} captured, {} unchanged",
        change.added, change.modified, change.deleted, change.captured, change.unchanged
    );
    if !change.sample_paths.is_empty() {
        println!("  sample: {}", change.sample_paths.join(", "));
    }
}

fn joined_or_dash(values: &[String]) -> String {
    if values.is_empty() {
        "-".to_string()
    } else {
        values.join(", ")
    }
}

fn changed_word(changed: bool) -> &'static str {
    if changed { "changed" } else { "unchanged" }
}
